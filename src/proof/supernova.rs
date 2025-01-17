#![allow(non_snake_case)]
use std::marker::PhantomData;
use std::ops::Index;

use abomonation::Abomonation;
use tracing::{debug, info};

use bellpepper_core::{num::AllocatedNum, ConstraintSystem, SynthesisError};
use nova::{
    self,
    supernova::{self, error::SuperNovaError, NonUniformCircuit, RecursiveSNARK, RunningClaim},
    traits::{
        circuit_supernova::{StepCircuit, TrivialSecondaryCircuit},
        Group,
    },
};

use ff::{Field, PrimeField};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::circuit::MultiFrame;

use crate::coprocessor::Coprocessor;

use crate::error::ProofError;
use crate::eval::{lang::Lang, Frame, Meta, Witness, IO};
use crate::field::LurkField;
use crate::proof::nova::{CurveCycleEquipped, G1, G2};
use crate::proof::{Provable, Prover, PublicParameters};
use crate::ptr::Ptr;
use crate::store::Store;

/// Type alias for SuperNova Public Parameters with the curve cycle types defined above.
pub type SuperNovaPublicParams<F> = supernova::PublicParams<G1<F>, G2<F>>;

/// A struct that contains public parameters for the Nova proving system.
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct PublicParams<F, C: Coprocessor<F>>
where
    F: CurveCycleEquipped,
    // technical bounds that would disappear once associated_type_bounds stabilizes
    <<G1<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
{
    pp: SuperNovaPublicParams<F>,
    // SuperNova does not yet have a `CompressedSNARK`.
    // see https://github.com/lurk-lab/arecibo/issues/27
    // pk: ProverKey<G1<F>, G2<F>, C1<'a, F, C>, C2<F>, SS1<F>, SS2<F>>,
    // vk: VerifierKey<G1<F>, G2<F>, C1<'a, F, C>, C2<F>, SS1<F>, SS2<F>>,
    _p: PhantomData<C>,
}

impl<F: CurveCycleEquipped, C: Coprocessor<F>> Abomonation for PublicParams<F, C>
where
    <<G1<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
{
    unsafe fn entomb<W: std::io::Write>(&self, bytes: &mut W) -> std::io::Result<()> {
        self.pp.entomb(bytes)?;
        // self.pk.entomb(bytes)?;
        // self.vk.entomb(bytes)?;
        Ok(())
    }

    unsafe fn exhume<'b>(&mut self, mut bytes: &'b mut [u8]) -> Option<&'b mut [u8]> {
        let temp = bytes;
        bytes = self.pp.exhume(temp)?;
        // let temp = bytes;
        // bytes = self.pk.exhume(temp)?;
        // let temp = bytes;
        // bytes = self.vk.exhume(temp)?;
        Some(bytes)
    }

    fn extent(&self) -> usize {
        self.pp.extent() // + self.pk.extent() + self.vk.extent()
    }
}

/// An enum representing the two types of proofs that can be generated and verified.
#[derive(Serialize, Deserialize)]
pub enum Proof<F: CurveCycleEquipped, C: Coprocessor<F>>
where
    <<G1<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
    /// A proof for the intermediate steps of a recursive computation
    Recursive(Box<RecursiveSNARK<G1<F>, G2<F>>>),
    /// A proof for the final step of a recursive computation
    // Compressed(Box<CompressedSNARK<G1<F>, G2<F>, C1<'a, F, C>, C2<F>, SS1<F>, SS2<F>>>),
    Compressed(PhantomData<C>),
}

impl<F: CurveCycleEquipped, C: Coprocessor<F>> Proof<F, C>
where
    <<G1<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <F as PrimeField>::Repr: Abomonation,
    <<<F as CurveCycleEquipped>::G2 as Group>::Scalar as PrimeField>::Repr: Abomonation,
{
    /// Proves the computation recursively, generating a recursive SNARK proof.
    #[tracing::instrument(skip_all, name = "Proof::prove_recursively")]
    pub fn prove_recursively(
        _pp: Option<&PublicParams<F, C>>,
        _store: &Store<F>,
        nivc_steps: &NIVCSteps<'_, G1<F>, C>,
        reduction_count: usize,
        z0: Vec<F>,
        lang: Arc<Lang<F, C>>,
    ) -> Result<Self, ProofError> {
        // Is this assertion strictly necessary?
        assert!(nivc_steps.num_steps() != 0);
        // NOTE: The `Meta::Lurk` in the blank step is used as a default. It might be worth more explicitly supporting
        // an undifferentiated 'stem cell' blank `NonUniformCircuit`, for clarity.
        let folding_config = Arc::new(FoldingConfig::new_nivc(lang, reduction_count));
        let blank_step = NIVCStep::blank(folding_config, Meta::Lurk);

        info!("setting up running claims");
        let running_claims = blank_step.setup_running_claims();
        info!("running claim setup complete");
        let mut recursive_snark_option: Option<RecursiveSNARK<G1<F>, G2<F>>> = None;

        let z0_primary = z0;
        let z0_secondary = Self::z0_secondary();

        let mut last_running_claim = &running_claims[nivc_steps.steps[0].circuit_index()];

        for (i, step) in nivc_steps.steps.iter().enumerate() {
            info!("prove_recursively, step {i}");
            let augmented_circuit_index = step.circuit_index();
            let program_counter = F::from(augmented_circuit_index as u64);

            let mut recursive_snark = recursive_snark_option.clone().unwrap_or_else(|| {
                info!("iter_base_step {i}");
                RecursiveSNARK::iter_base_step(
                    &running_claims[augmented_circuit_index],
                    step,
                    running_claims.digest(),
                    Some(program_counter),
                    augmented_circuit_index,
                    step.num_circuits(),
                    &z0_primary,
                    &z0_secondary,
                )
                .unwrap()
            });

            info!("prove_step {i}");

            recursive_snark
                .prove_step(
                    &running_claims[augmented_circuit_index],
                    step,
                    &z0_primary,
                    &z0_secondary,
                )
                .unwrap();
            info!("verify step {i}");
            recursive_snark
                .verify(
                    &running_claims[augmented_circuit_index],
                    &z0_primary,
                    &z0_secondary,
                )
                .unwrap();
            recursive_snark_option = Some(recursive_snark);

            last_running_claim = &running_claims[augmented_circuit_index];
        }

        // TODO: return `last_running_claim` somehow, so it can be used to verify.
        let _ = last_running_claim;

        // This probably should be made unnecessary.
        Ok(Self::Recursive(Box::new(
            recursive_snark_option.expect("RecursiveSNARK missing"),
        )))
    }

    /// Verifies the proof given the claim, which (for now), contains the public parameters.
    pub fn verify(
        &self,
        claim: &RunningClaim<
            G1<F>,
            G2<F>,
            NIVCStep<'_, F, C>,
            TrivialSecondaryCircuit<<G2<F> as Group>::Scalar>,
        >,
        _pp: Option<&PublicParams<F, C>>,
        _num_steps: usize,
        z0: &[F],
        zi: &[F],
    ) -> Result<bool, SuperNovaError> {
        let (z0_primary, _zi_primary) = (z0, zi);
        let z0_secondary = Self::z0_secondary();

        match self {
            Self::Recursive(p) => p.verify(claim, z0_primary, &z0_secondary),
            Self::Compressed(_) => unimplemented!(),
        }?;
        Ok(true)
    }

    fn z0_secondary() -> Vec<<F::G2 as Group>::Scalar> {
        vec![<G2<F> as Group>::Scalar::ZERO]
    }
}

// /// Generates the public parameters for the Nova proving system.
// pub fn public_params<'a, F: CurveCycleEquipped, C: Coprocessor<F>>(
//     num_iters_per_step: usize,
//     lang: Arc<Lang<F, C>>,
// ) -> PublicParams<'a, F, C>
// where
//     <<G1<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
//     <<G2<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
// {
//     let (circuit_primary, circuit_secondary) = C1::circuits(num_iters_per_step, lang);

//     let commitment_size_hint1 = <SS1<F> as RelaxedR1CSSNARKTrait<G1<F>>>::commitment_key_floor();
//     let commitment_size_hint2 = <SS2<F> as RelaxedR1CSSNARKTrait<G2<F>>>::commitment_key_floor();

//     let pp = nova::PublicParams::setup(
//         &circuit_primary,
//         &circuit_secondary,
//         Some(commitment_size_hint1),
//         Some(commitment_size_hint2),
//     );
//     let (pk, vk) = CompressedSNARK::setup(&pp).unwrap();
//     PublicParams { pp, pk, vk }
// }

/// A struct for the Nova prover that operates on field elements of type `F`.
#[derive(Debug)]
pub struct SuperNovaProver<F: CurveCycleEquipped, C: Coprocessor<F>> {
    // `reduction_count` specifies the number of small-step reductions are performed in each recursive step of the primary Lurk circuit.
    reduction_count: usize,
    lang: Lang<F, C>,
}

impl<F: CurveCycleEquipped, C: Coprocessor<F>> PublicParameters for PublicParams<F, C>
where
    <<G1<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
}

impl<'a, F: CurveCycleEquipped, C: Coprocessor<F> + 'a> Prover<'a, F, C, MultiFrame<'a, F, C>>
    for SuperNovaProver<F, C>
where
    <<G1<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
    type PublicParams = PublicParams<F, C>;
    fn new(reduction_count: usize, lang: Lang<F, C>) -> Self {
        SuperNovaProver::<F, C> {
            reduction_count,
            lang,
        }
    }
    fn reduction_count(&self) -> usize {
        self.reduction_count
    }

    fn lang(&self) -> &Lang<F, C> {
        &self.lang
    }
}

impl<F: CurveCycleEquipped, C: Coprocessor<F>> SuperNovaProver<F, C>
where
    <<G1<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as ff::PrimeField>::Repr: Abomonation,
{
    /// Proves the computation given the public parameters, frames, and store.
    pub fn prove<'a>(
        &'a self,
        pp: Option<&PublicParams<F, C>>,
        frames: &[Frame<IO<F>, Witness<F>, F, C>],
        store: &'a mut Store<F>,
        lang: Arc<Lang<F, C>>,
    ) -> Result<(Proof<F, C>, Vec<F>, Vec<F>, usize), ProofError> {
        let z0 = frames[0].input.to_vector(store)?;
        let zi = frames.last().unwrap().output.to_vector(store)?;
        let folding_config = Arc::new(FoldingConfig::new_nivc(lang.clone(), self.reduction_count));

        let nivc_steps =
            NIVCSteps::from_frames(self.reduction_count(), frames, store, folding_config);

        let num_steps = nivc_steps.num_steps();
        let proof = Proof::prove_recursively(
            pp,
            store,
            &nivc_steps,
            self.reduction_count,
            z0.clone(),
            lang,
        )?;

        Ok((proof, z0, zi, num_steps))
    }

    /// Evaluates and proves the computation given the public parameters, expression, environment, and store.
    pub fn evaluate_and_prove<'a>(
        &'a self,
        pp: Option<&PublicParams<F, C>>,
        expr: Ptr<F>,
        env: Ptr<F>,
        store: &'a mut Store<F>,
        limit: usize,
        lang: Arc<Lang<F, C>>,
    ) -> Result<(Proof<F, C>, Vec<F>, Vec<F>, usize), ProofError> {
        let frames = self.get_evaluation_frames(expr, env, store, limit, lang.clone())?;
        info!("got {} evaluation frames", frames.len());
        self.prove(pp, &frames, store, lang)
    }
}

#[derive(Clone, Debug)]
/// Folding configuration specifies `Lang` and can be either `IVC` or `NIVC`.
// NOTE: This is somewhat trivial now, but will likely become more elaborate as NIVC configuration becomes more flexible.
pub enum FoldingConfig<F: LurkField, C: Coprocessor<F>> {
    // TODO: maybe (lang, reduction_count) should be a common struct.
    /// IVC: a single circuit implementing the `Lang`'s reduction will be used for every folding step
    IVC(Arc<Lang<F, C>>, usize),
    /// NIVC: each folding step will use one of a fixed set of circuits which together implement the `Lang`'s reduction.
    NIVC(Arc<Lang<F, C>>, usize),
}

impl<F: LurkField, C: Coprocessor<F>> FoldingConfig<F, C> {
    /// Create a new IVC config for `lang`.
    pub fn new_ivc(lang: Arc<Lang<F, C>>, reduction_count: usize) -> Self {
        Self::IVC(lang, reduction_count)
    }

    /// Create a new NIVC config for `lang`.
    pub fn new_nivc(lang: Arc<Lang<F, C>>, reduction_count: usize) -> Self {
        Self::NIVC(lang, reduction_count)
    }

    /// Return the circuit index assigned in this `FoldingConfig` to circuits tagged with this `meta`.
    pub fn circuit_index(&self, meta: &Meta<F>) -> usize {
        match self {
            Self::IVC(_, _) => 0,
            Self::NIVC(lang, _) => match meta {
                Meta::Lurk => 0,
                Meta::Coprocessor(z_ptr) => lang.get_index(z_ptr).unwrap() + 1,
            },
        }
    }

    /// Return the total number of NIVC circuits potentially required when folding programs described by this `FoldingConfig`.
    pub fn num_circuits(&self) -> usize {
        match self {
            Self::IVC(_, _) => 1,
            Self::NIVC(lang, _) => 1 + lang.coprocessor_count(),
        }
    }

    /// Return a reference to the contained `Lang`.
    pub fn lang(&self) -> &Arc<Lang<F, C>> {
        match self {
            Self::IVC(lang, _) | Self::NIVC(lang, _) => lang,
        }
    }
    /// Return contained reduction count.
    pub fn reduction_count(&self) -> usize {
        match self {
            Self::IVC(_, rc) | Self::NIVC(_, rc) => *rc,
        }
    }
}

impl<'a, F: LurkField, C: Coprocessor<F>> MultiFrame<'a, F, C> {
    /// Return the circuit index assigned to this `MultiFrame`'s inner computation, as labeled by its `Meta`, and determined by its `FoldingConfig`.
    pub fn circuit_index(&self) -> usize {
        debug!(
            "getting circuit_index for {:?}: {}",
            &self.meta,
            self.folding_config.circuit_index(&self.meta)
        );
        self.folding_config.circuit_index(&self.meta)
    }
}

#[derive(Clone, Debug)]
/// One step of an NIVC computation
pub struct NIVCStep<'a, F: LurkField, C: Coprocessor<F>> {
    multiframe: MultiFrame<'a, F, C>,
    next: Option<MultiFrame<'a, F, C>>,
    _p: PhantomData<F>,
}

impl<'a, 'b, F: LurkField, C: Coprocessor<F>> NIVCStep<'a, F, C>
where
    'b: 'a,
{
    fn new(multiframe: MultiFrame<'b, F, C>) -> Self {
        Self {
            multiframe,
            next: None,
            _p: Default::default(),
        }
    }

    fn blank(folding_config: Arc<FoldingConfig<F, C>>, meta: Meta<F>) -> Self {
        let multiframe = MultiFrame::blank(folding_config, meta);
        Self::new(multiframe)
    }

    fn lang(&self) -> Arc<Lang<F, C>> {
        self.multiframe.folding_config.lang().clone()
    }

    fn meta(&self) -> Meta<F> {
        self.multiframe.meta
    }

    fn folding_config(&self) -> Arc<FoldingConfig<F, C>> {
        self.multiframe.folding_config.clone()
    }
}

/// Implement `supernova::StepCircuit` for `MultiFrame`. This is the universal Lurk circuit that will be included as the
/// first circuit (index 0) of every Lurk NIVC circuit set.
impl<F: LurkField, C: Coprocessor<F>> StepCircuit<F> for NIVCStep<'_, F, C> {
    fn arity(&self) -> usize {
        MultiFrame::<'_, F, C>::public_input_size() / 2
    }

    fn circuit_index(&self) -> usize {
        self.multiframe.circuit_index()
    }

    fn synthesize<CS>(
        &self,
        cs: &mut CS,
        pc: std::option::Option<&AllocatedNum<F>>,
        z: &[AllocatedNum<F>],
    ) -> Result<(std::option::Option<AllocatedNum<F>>, Vec<AllocatedNum<F>>), SynthesisError>
    where
        CS: ConstraintSystem<F>,
    {
        if let Some(pc) = pc {
            if pc.get_value() == Some(F::ZERO) {
                debug!("synthesizing StepCircuit for Lurk");
            } else {
                debug!(
                    "synthesizing StepCircuit for Coprocessor with pc: {:?}",
                    pc.get_value()
                );
            }
        }
        let output = <MultiFrame<'_, F, C> as nova::traits::circuit::StepCircuit<F>>::synthesize(
            &self.multiframe,
            cs,
            z,
        )?;

        let next_pc = AllocatedNum::alloc_infallible(&mut cs.namespace(|| "next_pc"), || {
            self.next
                .as_ref()
                // This is missing in the case of a final `MultiFrame`. The Lurk circuit is defined to always have index
                // 0, so it is a good default in this case.
                .map_or(F::ZERO, |x| F::from(x.circuit_index() as u64))
        });
        debug!("synthesizing with next_pc: {:?}", next_pc.get_value());

        Ok((Some(next_pc), output))
    }
}

/// All steps of an NIVC computation
pub struct NIVCSteps<'a, G: Group, C: Coprocessor<G::Scalar>>
where
    G::Scalar: LurkField,
{
    steps: Vec<NIVCStep<'a, G::Scalar, C>>,
}
impl<'a, G1, F, C1> Index<usize> for NIVCSteps<'a, G1, C1>
where
    C1: Coprocessor<F> + 'a,
    G1: Group<Scalar = F> + 'a,
    F: LurkField,
{
    type Output = NIVCStep<'a, G1::Scalar, C1>;

    fn index(&self, idx: usize) -> &<Self as std::ops::Index<usize>>::Output {
        &self.steps[idx]
    }
}

impl<'a, G1, F, C1> NIVCSteps<'a, G1, C1>
where
    C1: Coprocessor<F>,
    G1: Group<Scalar = F>,
    F: LurkField,
{
    /// Number of NIVC steps contained.
    pub fn num_steps(&self) -> usize {
        self.steps.len()
    }
    /// Separate frames according to NIVC circuit requirements.
    pub fn from_frames(
        count: usize,
        frames: &[Frame<IO<F>, Witness<F>, F, C1>],
        store: &'a Store<F>,
        folding_config: Arc<FoldingConfig<F, C1>>,
    ) -> Self {
        let mut steps = Vec::new();
        let mut last_meta = frames[0].meta;
        let mut consecutive_frames = Vec::new();

        for frame in frames {
            if frame.meta == last_meta {
                let padding_frame = frame.clone();
                consecutive_frames.push(padding_frame);
            } else {
                if last_meta == Meta::Lurk {
                    consecutive_frames.push(frame.clone());
                }
                let new_steps = MultiFrame::from_frames(
                    if last_meta == Meta::Lurk { count } else { 1 },
                    &consecutive_frames,
                    store,
                    folding_config.clone(),
                )
                .into_iter()
                .map(NIVCStep::<'_, F, C1>::new);

                steps.extend(new_steps);
                consecutive_frames.truncate(0);
                consecutive_frames.push(frame.clone());
                last_meta = frame.meta;
            }
        }

        // TODO: refactor
        if !consecutive_frames.is_empty() {
            let new_steps =
                MultiFrame::from_frames(count, &consecutive_frames, store, folding_config)
                    .into_iter()
                    .map(NIVCStep::<'_, F, C1>::new);

            steps.extend(new_steps);
        }

        if !steps.is_empty() {
            let penultimate = steps.len() - 1;
            for i in 0..(penultimate - 1) {
                steps[i].next = Some(steps[i + 1].multiframe.clone());
            }
        }
        Self { steps }
    }
}

impl<'a, F, C1> NonUniformCircuit<G1<F>, G2<F>, NIVCStep<'a, F, C1>> for NIVCStep<'a, F, C1>
where
    <<G1<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2<F> as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <F as PrimeField>::Repr: Abomonation,
    C1: Coprocessor<F>,
    F: CurveCycleEquipped + LurkField,
    <<<F as CurveCycleEquipped>::G2 as Group>::Scalar as PrimeField>::Repr: Abomonation,
{
    fn num_circuits(&self) -> usize {
        assert!(self.meta().is_lurk());
        self.lang().coprocessor_count() + 1
    }

    fn primary_circuit(&self, circuit_index: usize) -> Self {
        debug!(
            "getting primary_circuit for index {circuit_index} and Meta: {:?}",
            self.meta()
        );
        if circuit_index == 0 {
            debug!("using Lurk circuit");
            return self.clone();
        };
        if let Some(z_ptr) = self.lang().get_coprocessor_z_ptr(circuit_index - 1) {
            let meta = Meta::Coprocessor(*z_ptr);
            debug!(
                "using coprocessor {} with meta: {:?}",
                circuit_index - 1,
                meta
            );
            Self::blank(self.folding_config(), meta)
        } else {
            debug!("unsupported primary circuit index: {circuit_index}");
            panic!("unsupported primary circuit index")
        }
    }
}
