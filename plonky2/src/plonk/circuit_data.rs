use std::collections::BTreeMap;
use std::ops::{Range, RangeFrom};

use anyhow::Result;
use plonky2_field::extension_field::Extendable;
use plonky2_field::fft::FftRootTable;

use crate::field::field_types::Field;
use crate::fri::oracle::PolynomialBatch;
use crate::fri::reduction_strategies::FriReductionStrategy;
use crate::fri::structure::{
    FriBatchInfo, FriBatchInfoTarget, FriInstanceInfo, FriInstanceInfoTarget, FriPolynomialInfo,
};
use crate::fri::{FriConfig, FriParams};
use crate::gates::gate::PrefixedGate;
use crate::hash::hash_types::{MerkleCapTarget, RichField};
use crate::hash::merkle_tree::MerkleCap;
use crate::iop::ext_target::ExtensionTarget;
use crate::iop::generator::WitnessGenerator;
use crate::iop::target::Target;
use crate::iop::witness::PartialWitness;
use crate::plonk::circuit_builder::CircuitBuilder;
use crate::plonk::config::{GenericConfig, Hasher};
use crate::plonk::plonk_common::{PlonkOracle, FRI_ORACLES};
use crate::plonk::proof::{CompressedProofWithPublicInputs, ProofWithPublicInputs};
use crate::plonk::prover::prove;
use crate::plonk::verifier::verify;
use crate::util::marking::MarkedTargets;
use crate::util::timing::TimingTree;

#[derive(Clone, Debug)]
pub struct CircuitConfig {
    pub num_wires: usize,
    pub num_routed_wires: usize,
    pub constant_gate_size: usize,
    /// Whether to use a dedicated gate for base field arithmetic, rather than using a single gate
    /// for both base field and extension field arithmetic.
    pub use_base_arithmetic_gate: bool,
    pub security_bits: usize,
    /// The number of challenge points to generate, for IOPs that have soundness errors of (roughly)
    /// `degree / |F|`.
    pub num_challenges: usize,
    pub zero_knowledge: bool,

    pub fri_config: FriConfig,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        CircuitConfig::standard_recursion_config()
    }
}

impl CircuitConfig {
    pub fn rate(&self) -> f64 {
        1.0 / ((1 << self.fri_config.rate_bits) as f64)
    }

    pub fn num_advice_wires(&self) -> usize {
        self.num_wires - self.num_routed_wires
    }

    /// A typical recursion config, without zero-knowledge, targeting ~100 bit security.
    pub fn standard_recursion_config() -> Self {
        Self {
            num_wires: 135,
            num_routed_wires: 80,
            constant_gate_size: 5,
            use_base_arithmetic_gate: true,
            security_bits: 100,
            num_challenges: 2,
            zero_knowledge: false,
            fri_config: FriConfig {
                rate_bits: 3,
                cap_height: 4,
                proof_of_work_bits: 16,
                reduction_strategy: FriReductionStrategy::ConstantArityBits(4, 5),
                num_query_rounds: 28,
            },
        }
    }

    pub fn standard_recursion_zk_config() -> Self {
        CircuitConfig {
            zero_knowledge: true,
            ..Self::standard_recursion_config()
        }
    }
}

/// Circuit data required by the prover or the verifier.
pub struct CircuitData<F: RichField + Extendable<D>, C: GenericConfig<D, F = F>, const D: usize> {
    pub(crate) prover_only: ProverOnlyCircuitData<F, C, D>,
    pub(crate) verifier_only: VerifierOnlyCircuitData<C, D>,
    pub(crate) common: CommonCircuitData<F, C, D>,
}

impl<F: RichField + Extendable<D>, C: GenericConfig<D, F = F>, const D: usize>
    CircuitData<F, C, D>
{
    pub fn prove(&self, inputs: PartialWitness<F>) -> Result<ProofWithPublicInputs<F, C, D>> {
        prove(
            &self.prover_only,
            &self.common,
            inputs,
            &mut TimingTree::default(),
        )
    }

    pub fn verify(&self, proof_with_pis: ProofWithPublicInputs<F, C, D>) -> Result<()> {
        verify(proof_with_pis, &self.verifier_only, &self.common)
    }

    pub fn verify_compressed(
        &self,
        compressed_proof_with_pis: CompressedProofWithPublicInputs<F, C, D>,
    ) -> Result<()> {
        compressed_proof_with_pis.verify(&self.verifier_only, &self.common)
    }
}

/// Circuit data required by the prover. This may be thought of as a proving key, although it
/// includes code for witness generation.
///
/// The goal here is to make proof generation as fast as we can, rather than making this prover
/// structure as succinct as we can. Thus we include various precomputed data which isn't strictly
/// required, like LDEs of preprocessed polynomials. If more succinctness was desired, we could
/// construct a more minimal prover structure and convert back and forth.
pub struct ProverCircuitData<
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    const D: usize,
> {
    pub(crate) prover_only: ProverOnlyCircuitData<F, C, D>,
    pub(crate) common: CommonCircuitData<F, C, D>,
}

impl<F: RichField + Extendable<D>, C: GenericConfig<D, F = F>, const D: usize>
    ProverCircuitData<F, C, D>
{
    pub fn prove(&self, inputs: PartialWitness<F>) -> Result<ProofWithPublicInputs<F, C, D>> {
        prove(
            &self.prover_only,
            &self.common,
            inputs,
            &mut TimingTree::default(),
        )
    }
}

/// Circuit data required by the prover.
#[derive(Debug)]
pub struct VerifierCircuitData<
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    const D: usize,
> {
    pub(crate) verifier_only: VerifierOnlyCircuitData<C, D>,
    pub(crate) common: CommonCircuitData<F, C, D>,
}

impl<F: RichField + Extendable<D>, C: GenericConfig<D, F = F>, const D: usize>
    VerifierCircuitData<F, C, D>
{
    pub fn verify(&self, proof_with_pis: ProofWithPublicInputs<F, C, D>) -> Result<()> {
        verify(proof_with_pis, &self.verifier_only, &self.common)
    }

    pub fn verify_compressed(
        &self,
        compressed_proof_with_pis: CompressedProofWithPublicInputs<F, C, D>,
    ) -> Result<()> {
        compressed_proof_with_pis.verify(&self.verifier_only, &self.common)
    }
}

/// Circuit data required by the prover, but not the verifier.
pub(crate) struct ProverOnlyCircuitData<
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    const D: usize,
> {
    pub generators: Vec<Box<dyn WitnessGenerator<F>>>,
    /// Generator indices (within the `Vec` above), indexed by the representative of each target
    /// they watch.
    pub generator_indices_by_watches: BTreeMap<usize, Vec<usize>>,
    /// Commitments to the constants polynomials and sigma polynomials.
    pub constants_sigmas_commitment: PolynomialBatch<F, C, D>,
    /// The transpose of the list of sigma polynomials.
    pub sigmas: Vec<Vec<F>>,
    /// Subgroup of order `degree`.
    pub subgroup: Vec<F>,
    /// Targets to be made public.
    pub public_inputs: Vec<Target>,
    /// A vector of marked targets. The values assigned to these targets will be displayed by the prover.
    pub marked_targets: Vec<MarkedTargets<D>>,
    /// A map from each `Target`'s index to the index of its representative in the disjoint-set
    /// forest.
    pub representative_map: Vec<usize>,
    /// Pre-computed roots for faster FFT.
    pub fft_root_table: Option<FftRootTable<F>>,
}

/// Circuit data required by the verifier, but not the prover.
#[derive(Debug)]
pub(crate) struct VerifierOnlyCircuitData<C: GenericConfig<D>, const D: usize> {
    /// A commitment to each constant polynomial and each permutation polynomial.
    pub(crate) constants_sigmas_cap: MerkleCap<C::F, C::Hasher>,
}

/// Circuit data required by both the prover and the verifier.
#[derive(Debug)]
pub struct CommonCircuitData<
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    const D: usize,
> {
    pub(crate) config: CircuitConfig,

    pub(crate) fri_params: FriParams,

    pub(crate) degree_bits: usize,

    /// The types of gates used in this circuit, along with their prefixes.
    pub(crate) gates: Vec<PrefixedGate<F, D>>,

    /// The degree of the PLONK quotient polynomial.
    pub(crate) quotient_degree_factor: usize,

    /// The largest number of constraints imposed by any gate.
    pub(crate) num_gate_constraints: usize,

    /// The number of constant wires.
    pub(crate) num_constants: usize,

    pub(crate) num_virtual_targets: usize,

    /// The `{k_i}` valued used in `S_ID_i` in Plonk's permutation argument.
    pub(crate) k_is: Vec<F>,

    /// The number of partial products needed to compute the `Z` polynomials and
    /// the number of original elements consumed in `partial_products()`.
    pub(crate) num_partial_products: (usize, usize),

    /// A digest of the "circuit" (i.e. the instance, minus public inputs), which can be used to
    /// seed Fiat-Shamir.
    pub(crate) circuit_digest: <<C as GenericConfig<D>>::Hasher as Hasher<F>>::Hash,
}

impl<F: RichField + Extendable<D>, C: GenericConfig<D, F = F>, const D: usize>
    CommonCircuitData<F, C, D>
{
    pub fn degree(&self) -> usize {
        1 << self.degree_bits
    }

    pub fn lde_size(&self) -> usize {
        1 << (self.degree_bits + self.config.fri_config.rate_bits)
    }

    pub fn lde_generator(&self) -> F {
        F::primitive_root_of_unity(self.degree_bits + self.config.fri_config.rate_bits)
    }

    pub fn constraint_degree(&self) -> usize {
        self.gates
            .iter()
            .map(|g| g.gate.0.degree())
            .max()
            .expect("No gates?")
    }

    pub fn quotient_degree(&self) -> usize {
        self.quotient_degree_factor * self.degree()
    }

    /// Range of the constants polynomials in the `constants_sigmas_commitment`.
    pub fn constants_range(&self) -> Range<usize> {
        0..self.num_constants
    }

    /// Range of the sigma polynomials in the `constants_sigmas_commitment`.
    pub fn sigmas_range(&self) -> Range<usize> {
        self.num_constants..self.num_constants + self.config.num_routed_wires
    }

    /// Range of the `z`s polynomials in the `zs_partial_products_commitment`.
    pub fn zs_range(&self) -> Range<usize> {
        0..self.config.num_challenges
    }

    /// Range of the partial products polynomials in the `zs_partial_products_commitment`.
    pub fn partial_products_range(&self) -> RangeFrom<usize> {
        self.config.num_challenges..
    }

    pub(crate) fn get_fri_instance(&self, zeta: F::Extension) -> FriInstanceInfo<F, D> {
        // All polynomials are opened at zeta.
        let zeta_batch = FriBatchInfo {
            point: zeta,
            polynomials: self.fri_all_polys(),
        };

        // The Z polynomials are also opened at g * zeta.
        let g = F::Extension::primitive_root_of_unity(self.degree_bits);
        let zeta_right = g * zeta;
        let zeta_right_batch = FriBatchInfo {
            point: zeta_right,
            polynomials: self.fri_zs_polys(),
        };

        let openings = vec![zeta_batch, zeta_right_batch];
        FriInstanceInfo {
            oracles: FRI_ORACLES.to_vec(),
            batches: openings,
        }
    }

    pub(crate) fn get_fri_instance_target(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        zeta: ExtensionTarget<D>,
    ) -> FriInstanceInfoTarget<D> {
        // All polynomials are opened at zeta.
        let zeta_batch = FriBatchInfoTarget {
            point: zeta,
            polynomials: self.fri_all_polys(),
        };

        // The Z polynomials are also opened at g * zeta.
        let g = F::primitive_root_of_unity(self.degree_bits);
        let zeta_right = builder.mul_const_extension(g, zeta);
        let zeta_right_batch = FriBatchInfoTarget {
            point: zeta_right,
            polynomials: self.fri_zs_polys(),
        };

        let openings = vec![zeta_batch, zeta_right_batch];
        FriInstanceInfoTarget {
            oracles: FRI_ORACLES.to_vec(),
            batches: openings,
        }
    }

    fn fri_preprocessed_polys(&self) -> Vec<FriPolynomialInfo> {
        let num_preprocessed_polys = self.sigmas_range().end;
        FriPolynomialInfo::from_range(
            PlonkOracle::CONSTANTS_SIGMAS.index,
            0..num_preprocessed_polys,
        )
    }

    fn fri_wire_polys(&self) -> Vec<FriPolynomialInfo> {
        let num_wire_polys = self.config.num_wires;
        FriPolynomialInfo::from_range(PlonkOracle::WIRES.index, 0..num_wire_polys)
    }

    fn fri_zs_partial_products_polys(&self) -> Vec<FriPolynomialInfo> {
        let num_zs_partial_products_polys =
            self.config.num_challenges * (1 + self.num_partial_products.0);
        FriPolynomialInfo::from_range(
            PlonkOracle::ZS_PARTIAL_PRODUCTS.index,
            0..num_zs_partial_products_polys,
        )
    }

    fn fri_zs_polys(&self) -> Vec<FriPolynomialInfo> {
        FriPolynomialInfo::from_range(PlonkOracle::ZS_PARTIAL_PRODUCTS.index, self.zs_range())
    }

    fn fri_quotient_polys(&self) -> Vec<FriPolynomialInfo> {
        let num_quotient_polys = self.config.num_challenges * self.quotient_degree_factor;
        FriPolynomialInfo::from_range(PlonkOracle::QUOTIENT.index, 0..num_quotient_polys)
    }

    fn fri_all_polys(&self) -> Vec<FriPolynomialInfo> {
        [
            self.fri_preprocessed_polys(),
            self.fri_wire_polys(),
            self.fri_zs_partial_products_polys(),
            self.fri_quotient_polys(),
        ]
        .concat()
    }
}

/// The `Target` version of `VerifierCircuitData`, for use inside recursive circuits. Note that this
/// is intentionally missing certain fields, such as `CircuitConfig`, because we support only a
/// limited form of dynamic inner circuits. We can't practically make things like the wire count
/// dynamic, at least not without setting a maximum wire count and paying for the worst case.
pub struct VerifierCircuitTarget {
    /// A commitment to each constant polynomial and each permutation polynomial.
    pub(crate) constants_sigmas_cap: MerkleCapTarget,
}