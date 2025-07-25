use crate::{
    Claim, Prover, Tensor,
    commit::same_poly,
    iop::{context::ShapeStep, verifier::Verifier},
    layers::LayerProof,
    lookup::{
        context::LookupWitnessGen,
        logup_gkr::{prover::batch_prove as logup_batch_prove, verifier::verify_logup_proof},
    },
    model::StepData,
    padding::PaddingMode,
    quantization,
};
use anyhow::{Result, anyhow, ensure};
use ff::Field;
use ff_ext::ExtensionField;
use gkr::util::ceil_log2;
use itertools::Itertools;
use multilinear_extensions::mle::IntoMLE;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    ops::{Add, Mul, Sub},
};
use tracing::warn;
use transcript::Transcript;

use crate::{
    Element,
    commit::precommit::PolyID,
    iop::context::ContextAux,
    lookup::{context::TableType, logup_gkr::structs::LogUpProof},
    quantization::Fieldizer,
};

use super::{
    LayerCtx,
    provable::{Evaluate, LayerOut, NodeId, OpInfo, PadOp, ProvableOp, ProveInfo, VerifiableCtx},
};

enum RequantResult {
    Ok(Element),
    OutOfRange(Element),
}

/// Information about a requantization step:
/// * what is the range of the input data
/// * what should be the shift to get back data in range within QuantInteger range
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Copy, PartialOrd)]
pub struct Requant {
    // what is the shift that needs to be applied to requantize input number to the correct range of QuantInteger.
    pub right_shift: usize,
    // this is the range we expect the values to be in pre shift
    // This is a magnitude: e.g. [-4;8] gives range = 12.
    // This is to make sure to offset the values to be positive integers before doing the shift
    // That info is used to construct a lookup table for the requantization so the size of the lookup table
    // is directly correlated to the range of the input data.
    pub range: usize,
    /// The range we want the values to be in post requantizing
    pub after_range: usize,
    /// TEST ONLY: this can be given to simulate a perfect requantization during inference. Note that it CAN NOT
    /// be proven currently.
    pub multiplier: Option<f32>,
}

/// Info related to the lookup protocol necessary to requantize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequantCtx {
    pub requant: Requant,
    pub poly_id: PolyID,
    pub num_vars: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RequantProof<E: ExtensionField>
where
    E::BaseField: Serialize + DeserializeOwned,
{
    /// proof for the accumulation of the claim from activation + claim from lookup for the same poly
    /// e.g. the "link" between an activation and requant layer
    pub(crate) io_accumulation: same_poly::Proof<E>,
    /// the lookup proof for the requantization
    pub(crate) lookup: LogUpProof<E>,
}

const IS_PROVABLE: bool = true;

impl OpInfo for Requant {
    fn output_shapes(
        &self,
        input_shapes: &[Vec<usize>],
        _padding_mode: PaddingMode,
    ) -> Vec<Vec<usize>> {
        input_shapes.to_vec() // preserve the input shape
    }

    fn num_outputs(&self, num_inputs: usize) -> usize {
        num_inputs
    }

    fn describe(&self) -> String {
        format!(
            "Requant: shift: {}, offset: 2^{}",
            self.right_shift,
            (self.range << 1).ilog2() as usize,
        )
    }

    fn is_provable(&self) -> bool {
        IS_PROVABLE
    }
}

impl Evaluate<Element> for Requant {
    fn evaluate<E: ExtensionField>(
        &self,
        inputs: &[&Tensor<Element>],
        _unpadded_input_shapes: Vec<Vec<usize>>,
    ) -> Result<LayerOut<Element, E>> {
        ensure!(
            inputs.len() == 1,
            "Found more than 1 input when evaluating requant layer"
        );
        let input = inputs[0];
        Ok(LayerOut::from_vec(vec![self.op(input)?]))
    }
}

impl<E> ProveInfo<E> for Requant
where
    E: ExtensionField + DeserializeOwned,
    E::BaseField: Serialize + DeserializeOwned,
{
    fn step_info(&self, id: PolyID, mut aux: ContextAux) -> Result<(LayerCtx<E>, ContextAux)> {
        aux.tables.insert(TableType::Range);
        let num_vars = aux
            .last_output_shape
            .iter_mut()
            .fold(Ok(None), |expected_num_vars, shape| {
                let num_vars = shape.iter().map(|dim| ceil_log2(*dim)).sum::<usize>();
                if let Some(vars) = expected_num_vars? {
                    ensure!(
                        vars == num_vars,
                        "All input shapes for requant layer must have the same number of variables"
                    );
                }
                Ok(Some(num_vars))
            })?
            .expect("No input shape found for requant layer?");
        Ok((
            LayerCtx::Requant(RequantCtx {
                requant: *self,
                poly_id: id,
                num_vars,
            }),
            aux,
        ))
    }
}

impl PadOp for Requant {}

impl<E> ProvableOp<E> for Requant
where
    E: ExtensionField,
    E::BaseField: Serialize + DeserializeOwned,
    E: Serialize + DeserializeOwned,
{
    type Ctx = RequantCtx;

    fn prove<T: Transcript<E>>(
        &self,
        id: NodeId,
        ctx: &Self::Ctx,
        last_claims: Vec<&Claim<E>>,
        step_data: &StepData<E, E>,
        prover: &mut Prover<E, T>,
    ) -> Result<Vec<Claim<E>>> {
        Ok(vec![self.prove_step(
            prover,
            last_claims[0],
            &step_data.outputs.outputs()[0].get_data(),
            ctx,
            id,
        )?])
    }

    fn gen_lookup_witness(
        &self,
        id: NodeId,
        gen: &mut LookupWitnessGen<E>,
        step_data: &StepData<Element, E>,
    ) -> Result<()> {
        ensure!(
            step_data.inputs.len() == 1,
            "Found more than 1 input in inference step of requant layer"
        );
        ensure!(
            step_data.outputs.outputs().len() == 1,
            "Found more than 1 output in inference step of requant layer"
        );

        gen.tables.insert(TableType::Range);
        let table_lookup_map = gen
            .lookups
            .entry(TableType::Range)
            .or_insert_with(|| HashMap::default());

        let (merged_lookups, column_evals) =
            self.lookup_witness::<E>(step_data.inputs[0].get_data());
        merged_lookups
            .into_iter()
            .for_each(|val| *table_lookup_map.entry(val).or_insert(0u64) += 1);

        gen.polys_with_id.push((
            id as PolyID,
            step_data.outputs.outputs()[0]
                .get_data()
                .iter()
                .map(Fieldizer::<E>::to_field)
                .collect(),
        ));

        gen.lookups_no_challenges
            .insert(id, (column_evals, 1, TableType::Range));

        Ok(())
    }
}

impl OpInfo for RequantCtx {
    fn output_shapes(
        &self,
        input_shapes: &[Vec<usize>],
        _padding_mode: PaddingMode,
    ) -> Vec<Vec<usize>> {
        input_shapes.to_vec()
    }

    fn num_outputs(&self, num_inputs: usize) -> usize {
        Requant::num_outputs(num_inputs)
    }

    fn describe(&self) -> String {
        format!(
            "Requant ctx: shift: {}, offset: 2^{}",
            self.requant.right_shift,
            (self.requant.range << 1).ilog2() as usize,
        )
    }

    fn is_provable(&self) -> bool {
        IS_PROVABLE
    }
}

impl<E> VerifiableCtx<E> for RequantCtx
where
    E: ExtensionField,
    E::BaseField: Serialize + DeserializeOwned,
    E: Serialize + DeserializeOwned,
{
    type Proof = RequantProof<E>;

    fn verify<T: Transcript<E>>(
        &self,
        proof: &Self::Proof,
        last_claims: &[&Claim<E>],
        verifier: &mut Verifier<E, T>,
        _shape_step: &ShapeStep,
    ) -> Result<Vec<Claim<E>>> {
        let (constant_challenge, column_separation_challenge) = verifier
            .challenge_storage
            .as_ref()
            .unwrap()
            .get_challenges_by_name(&TableType::Range.name())
            .ok_or(anyhow!(
                "Couldn't get challenges for LookupType: {}",
                TableType::Range.name()
            ))?;
        Ok(vec![self.verify_requant(
            verifier,
            last_claims[0],
            &proof,
            constant_challenge,
            column_separation_challenge,
        )?])
    }
}

impl Requant {
    fn num_outputs(num_inputs: usize) -> usize {
        num_inputs
    }

    pub fn new(min_value: usize, right_shift: usize) -> Self {
        Self {
            right_shift,
            range: min_value,
            after_range: *quantization::RANGE as usize,
            multiplier: None,
        }
    }
    pub fn set_test_multiplier(&mut self, multiplier: f32) {
        self.multiplier = Some(multiplier);
    }
    pub fn op(
        &self,
        input: &crate::tensor::Tensor<Element>,
    ) -> Result<crate::tensor::Tensor<Element>> {
        let mut not_ok_count = 0;
        let res = input
            .get_data()
            .iter()
            .map(|e| match self.apply(e) {
                RequantResult::Ok(res) => res,
                RequantResult::OutOfRange(res) => {
                    not_ok_count += 1;
                    res
                }
            })
            .collect_vec();
        // Debug information to uncomment when debugging scaling factor. Sometimes the right shift is too high
        // and we can observe values being null'd, e.g. set to 0 very quickly. Which messes up the distribution and
        // thus the inference.
        #[cfg(test)]
        {
            use statrs::statistics::{Data, Distribution};
            use tracing::debug;
            let d = Data::new(res.iter().map(|e| *e as f64).collect_vec());
            let stats = (d.mean().unwrap(), d.variance().unwrap());
            debug!(
                "AFTER REQUANT: shift {} : {:.2} % OUT OF RANGE (over total {})-> stats mean {:?} var {:?} \n\t->{:?}\n\t->{:?}",
                self.right_shift,
                not_ok_count as f32 / res.len() as f32 * 100.0,
                res.len(),
                stats.0,
                stats.1,
                &input.get_data()[..10.min(input.get_data().len())],
                &res[..10.min(res.len())],
            );
        }
        Ok(crate::tensor::Tensor::<Element>::new(
            input.get_shape(),
            res,
        ))
    }

    /// Applies requantization to a single element.
    ///
    /// This function performs the following steps:
    /// 1. Adds a large offset (max_bit) to ensure all values are positive
    /// 2. Right-shifts by the specified amount to reduce the bit width
    /// 3. Subtracts the shifted offset to restore the correct value range
    ///
    /// The result is a value that has been scaled down to fit within the
    /// target bit width while preserving the relative magnitudes.
    #[inline(always)]
    fn apply(&self, e: &Element) -> RequantResult {
        if let Some(_multiplier) = self.multiplier {
            panic!("this is only for test - disable manually");
            #[allow(unreachable_code)]
            let _res = (*e as f64 * _multiplier as f64).round() as Element;
            if !(_res >= *quantization::MIN && _res <= *quantization::MAX) {
                return RequantResult::OutOfRange(
                    _res.clamp(*quantization::MIN, *quantization::MAX),
                );
            } else {
                return RequantResult::Ok(_res);
            }
        }
        let max_bit = (self.range << 1) as Element;
        let tmp = e + max_bit;
        assert!(
            tmp >= 0,
            "offset is too small: element {} + {} (self.range << 1) = {}",
            e,
            self.range << 1,
            tmp
        );
        let tmp = tmp >> self.right_shift;
        let res = tmp - (max_bit >> self.right_shift);
        if !(res >= *quantization::MIN && res <= *quantization::MAX) {
            warn!("{} is NOT quantized correctly: res {}", e, res);
            RequantResult::OutOfRange(res)
        } else {
            RequantResult::Ok(res)
        }
    }

    pub fn write_to_transcript<E: ExtensionField, T: Transcript<E>>(&self, t: &mut T) {
        t.append_field_element(&E::BaseField::from(self.right_shift as u64));
        t.append_field_element(&E::BaseField::from(self.range as u64));
    }

    /// to_mle returns two polynomials:
    /// f_i: one containing the input column values
    /// f_o: one containing the output column values --> shifted to the right !
    /// TODO: have a "cache" of lookups for similar ranges
    pub fn to_mle<E: ExtensionField>(&self) -> Vec<E> {
        // TODO: make a +1 or -1 somewhere
        let min_range = -(self.after_range as Element) / 2;
        let max_range = (self.after_range as Element) / 2 - 1;
        (min_range..=max_range)
            .map(|i| i.to_field())
            .collect::<Vec<E>>()
    }
    /// Function that takes a list of field elements that need to be requantized (i.e. the output of a Dense layer)
    /// and splits each value into the correct decomposition for proving via lookups.
    pub fn prep_for_requantize<E: ExtensionField>(
        &self,
        input: &[Element],
    ) -> Vec<Vec<E::BaseField>> {
        // We calculate how many chunks we will split each entry of `input` into.
        // Since outputs of a layer are centered around zero (i.e. some are negative) in order for all the shifting
        // and the like to give the correct result we make sure that everything is positive.

        // The number of bits that get "sliced off" is equal to `self.right_shift`, we want to know how many limbs it takes to represent
        // this sliced off chunk in base `self.after_range`. To calculate this we perform ceiling division on `self.right_shift` by
        // `ceil_log2(self.after_range)` and then add one for the column that represents the output we will take to the next layer.
        let num_columns = (self.right_shift - 1) / ceil_log2(self.after_range) + 2;

        let num_vars = ceil_log2(input.len());

        let mut mle_evals = vec![vec![E::BaseField::ZERO; 1 << num_vars]; num_columns];

        // Bit mask for the bytes
        let bit_mask = self.after_range as i128 - 1;

        let max_bit = self.range << 1;
        let subtract = max_bit >> self.right_shift;

        input.iter().enumerate().for_each(|(index, val)| {
            let pre_shift = val + max_bit as i128;
            let tmp = pre_shift >> self.right_shift;
            let input = tmp - subtract as i128;
            let input_field: E = input.to_field();

            mle_evals[0][index] = input_field.as_bases()[0];
            // the value of an input should always be basefield elements

            // This leaves us with only the part that is "discarded"
            let mut remainder_vals = pre_shift - (tmp << self.right_shift);
            mle_evals
                .iter_mut()
                .skip(1)
                .rev()
                .for_each(|discarded_chunk| {
                    let chunk = remainder_vals & bit_mask;
                    let value = chunk as i128 - (self.after_range as i128 >> 1);
                    let field_elem: E = value.to_field();
                    discarded_chunk[index] = field_elem.as_bases()[0];
                    remainder_vals >>= self.after_range.ilog2();
                });
            debug_assert_eq!(remainder_vals, 0);
        });

        debug_assert!({
            input.iter().enumerate().fold(true, |acc, (i, value)| {
                let calc_evals = mle_evals
                    .iter()
                    .map(|col| E::from(col[i]))
                    .collect::<Vec<E>>();

                let field_value: E = value.to_field();
                acc & (self.recombine_claims(&calc_evals) == field_value)
            })
        });
        mle_evals
    }

    pub fn lookup_witness<E: ExtensionField>(
        &self,
        input: &[Element],
    ) -> (Vec<Element>, Vec<Vec<E::BaseField>>) {
        // We calculate how many chunks we will split each entry of `input` into.
        // Since outputs of a layer are centered around zero (i.e. some are negative) in order for all the shifting
        // and the like to give the correct result we make sure that everything is positive.

        // The number of bits that get "sliced off" is equal to `self.right_shift`, we want to know how many limbs it takes to represent
        // this sliced off chunk in base `self.after_range`. To calculate this we perform ceiling division on `self.right_shift` by
        // `ceil_log2(self.after_range)` and then add one for the column that represents the output we will take to the next layer.
        let num_columns = (self.right_shift - 1) / ceil_log2(self.after_range) + 2;

        let num_vars = ceil_log2(input.len());

        let mut lookups = vec![vec![0i128; 1 << num_vars]; num_columns];
        let mut lookups_field = vec![vec![E::BaseField::ZERO; 1 << num_vars]; num_columns];
        // Bit mask for the bytes
        let bit_mask = self.after_range.next_power_of_two() as i128 - 1;

        let max_bit = self.range << 1;
        let subtract = max_bit >> self.right_shift;

        input.iter().enumerate().for_each(|(index, val)| {
            let pre_shift = val + max_bit as i128;
            let tmp = pre_shift >> self.right_shift;
            let input = tmp - subtract as i128 + (self.after_range as i128 >> 1);
            let in_field: E = input.to_field();

            lookups[0][index] = input;
            lookups_field[0][index] = in_field.as_bases()[0];
            // the value of an input should always be basefield elements

            // This leaves us with only the part that is "discarded"
            let mut remainder_vals = pre_shift - (tmp << self.right_shift);
            lookups
                .iter_mut()
                .zip(lookups_field.iter_mut())
                .skip(1)
                .rev()
                .for_each(|(discarded_lookup_chunk, discarded_field_chunk)| {
                    let chunk = remainder_vals & bit_mask;
                    let value = chunk as i128;
                    let val_field: E = value.to_field();
                    discarded_lookup_chunk[index] = value;
                    discarded_field_chunk[index] = val_field.as_bases()[0];
                    remainder_vals >>= ceil_log2(self.after_range);
                });
            debug_assert_eq!(remainder_vals, 0);
        });

        debug_assert!({
            input.iter().enumerate().fold(true, |acc, (i, value)| {
                let calc_evals = lookups_field
                    .iter()
                    .map(|col| E::from(col[i]))
                    .collect::<Vec<E>>();

                let field_value: E = value.to_field();
                acc & (self.recombine_claims(&calc_evals) == field_value)
            })
        });
        (lookups.concat(), lookups_field)
    }

    /// Function to recombine claims of constituent MLEs into a single value to be used as the initial sumcheck evaluation
    /// of the subsequent proof.
    pub fn recombine_claims<
        E: From<u64> + Default + Add<Output = E> + Mul<Output = E> + Sub<Output = E> + Copy,
    >(
        &self,
        eval_claims: &[E],
    ) -> E {
        let max_bit = self.range << 1;
        let subtract = max_bit >> self.right_shift;

        // There may be padding claims so we only take the first `num_columns` claims

        let tmp_eval = E::from(1 << self.right_shift as u64)
            * (eval_claims[0] + E::from(subtract as u64) - E::from(self.after_range as u64 >> 1))
            + eval_claims.iter().skip(1).rev().enumerate().fold(
                E::default(),
                |acc, (i, &claim)| {
                    acc + E::from((self.after_range.next_power_of_two().pow(i as u32)) as u64)
                        * (claim)
                },
            );
        tmp_eval - E::from(max_bit as u64)
    }
    #[timed::timed_instrument(name = "Prover::prove_requant")]
    pub(crate) fn prove_step<E: ExtensionField, T: Transcript<E>>(
        &self,
        prover: &mut Prover<E, T>,
        last_claim: &Claim<E>,
        output: &[E],
        requant_info: &RequantCtx,
        id: NodeId,
    ) -> anyhow::Result<Claim<E>>
    where
        E: ExtensionField + Serialize + DeserializeOwned,
        E::BaseField: Serialize + DeserializeOwned,
    {
        let prover_info = prover.lookup_witness(id)?;

        // Run the lookup protocol and return the lookup proof
        let logup_proof = logup_batch_prove(&prover_info, prover.transcript)?;

        // We need to prove that the output of this step is the input to following activation function
        let mut same_poly_prover = same_poly::Prover::<E>::new(output.to_vec().into_mle());
        let same_poly_ctx = same_poly::Context::<E>::new(last_claim.point.len());
        same_poly_prover.add_claim(last_claim.clone())?;
        // For requant layers we have to extract the correct "chunk" from the list of claims
        let eval_claims = logup_proof
            .output_claims()
            .iter()
            .map(|claim| claim.eval)
            .collect::<Vec<E>>();

        let combined_eval = requant_info.requant.recombine_claims(&eval_claims);

        // Pass the eval associated with the poly used in the activation step to the same poly prover
        let first_claim = logup_proof
            .output_claims()
            .first()
            .ok_or(anyhow!("No claims found"))?;
        let point = first_claim.point.clone();

        let corrected_claim = Claim::<E> {
            point: point.clone(),
            eval: first_claim.eval - E::from((*quantization::RANGE / 2) as u64),
        };
        // println!("correct claim eval: {:?}", corrected_claim.eval);
        // println!(
        //    "output eval: {:?}",
        //    output.to_vec().into_mle().evaluate(&corrected_claim.point)
        //);
        // Add the claim used in the activation function
        same_poly_prover.add_claim(corrected_claim)?;
        let claim_acc_proof = same_poly_prover.prove(&same_poly_ctx, prover.transcript)?;

        prover
            .witness_prover
            .add_claim(requant_info.poly_id, claim_acc_proof.extract_claim())?;

        prover.push_proof(
            id,
            LayerProof::Requant(RequantProof {
                io_accumulation: claim_acc_proof,
                lookup: logup_proof,
            }),
        );

        Ok(Claim {
            point,
            eval: combined_eval,
        })
    }
}

impl RequantCtx {
    pub(crate) fn verify_requant<E: ExtensionField, T: Transcript<E>>(
        &self,
        verifier: &mut Verifier<E, T>,
        last_claim: &Claim<E>,
        proof: &RequantProof<E>,
        constant_challenge: E,
        column_separation_challenge: E,
    ) -> anyhow::Result<Claim<E>>
    where
        E::BaseField: Serialize + DeserializeOwned,
        E: Serialize + DeserializeOwned,
    {
        // 1. Verify the lookup proof
        let num_instances =
            (self.requant.right_shift - 1) / ceil_log2(self.requant.after_range) + 2;
        let verifier_claims = verify_logup_proof(
            &proof.lookup,
            num_instances,
            constant_challenge,
            column_separation_challenge,
            verifier.transcript,
        )?;

        // 2. Verify the accumulation proof from last_claim + lookup claim into the new claim
        let sp_ctx = same_poly::Context::<E>::new(self.num_vars);
        let mut sp_verifier = same_poly::Verifier::<E>::new(&sp_ctx);
        sp_verifier.add_claim(last_claim.clone())?;

        let first_claim = verifier_claims
            .claims()
            .first()
            .ok_or(anyhow::anyhow!("No claims found"))?;
        let point = first_claim.point.clone();
        // The first claim needs to be shifted down as we add a value to make sure that all its evals are in the range 0..1 << BIT_LEn
        let corrected_claim = Claim::<E>::new(
            point.clone(),
            first_claim.eval - E::from((*quantization::RANGE / 2) as u64),
        );
        sp_verifier.add_claim(corrected_claim)?;

        let new_output_claim = sp_verifier.verify(&proof.io_accumulation, verifier.transcript)?;
        // 3. Accumulate the new claim into the witness commitment protocol
        verifier
            .witness_verifier
            .add_claim(self.poly_id, new_output_claim)?;

        // Here we recombine all of the none dummy polynomials to get the actual claim that should be passed to the next layer
        let eval_claims = verifier_claims
            .claims()
            .iter()
            .map(|claim| claim.eval)
            .collect::<Vec<E>>();
        let eval = self.requant.recombine_claims(&eval_claims);
        // 4. return the input claim for to be proven at subsequent step
        Ok(Claim { point, eval })
    }
}
