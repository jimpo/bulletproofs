#![allow(non_snake_case)]

use clear_on_drop::clear::Clear;
use core::mem;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::MultiscalarMul;
use merlin::Transcript;

use super::{ConstraintSystem, LinearCombination, R1CSProof, RandomizedConstraintSystem, Variable};

use errors::R1CSError;
use generators::{BulletproofGens, PedersenGens};
use inner_product_proof::InnerProductProof;
use transcript::TranscriptProtocol;

/// A [`ConstraintSystem`] implementation for use by the prover.
///
/// The prover commits high-level variables and their blinding factors `(v, v_blinding)`,
/// allocates low-level variables and creates constraints in terms of these
/// high-level variables and low-level variables.
///
/// When all constraints are added, the proving code calls `prove`
/// which consumes the `Prover` instance, samples random challenges
/// that instantiate the randomized constraints, and creates a complete proof.
pub struct Prover<'a, 'b> {
    transcript: &'a mut Transcript,
    bp_gens: &'b BulletproofGens,
    pc_gens: &'b PedersenGens,
    /// The constraints accumulated so far.
    constraints: Vec<LinearCombination>,
    /// Stores assignments to the "left" of multiplication gates
    a_L: Vec<Scalar>,
    /// Stores assignments to the "right" of multiplication gates
    a_R: Vec<Scalar>,
    /// Stores assignments to the "output" of multiplication gates
    a_O: Vec<Scalar>,
    /// High-level witness data (value openings to V commitments)
    v: Vec<Scalar>,
    /// High-level witness data (blinding openings to V commitments)
    v_blinding: Vec<Scalar>,

    /// This list holds closures that will be called in the second phase of the protocol,
    /// when non-randomized variables are committed.
    deferred_constraints: Vec<Box<Fn(&mut RandomizingProver<'a, 'b>) -> Result<(), R1CSError>>>,
}

/// Prover in the randomizing phase.
///
/// Note: this type is exported because it is used to specify the associated type
/// in the public impl of a trait `ConstraintSystem`, which boils down to allowing compiler to
/// monomorphize the closures for the proving and verifying code.
/// However, this type cannot be instantiated by the user and therefore can only be used within
/// the callback provided to `specify_randomized_constraints`.
pub struct RandomizingProver<'a, 'b> {
    prover: Prover<'a, 'b>,
}

/// Overwrite secrets with null bytes when they go out of scope.
impl<'a, 'b> Drop for Prover<'a, 'b> {
    fn drop(&mut self) {
        self.v.clear();
        self.v_blinding.clear();

        // Important: due to how ClearOnDrop auto-implements InitializableFromZeroed
        // for T: Default, calling .clear() on Vec compiles, but does not
        // clear the content. Instead, it only clears the Vec's header.
        // Clearing the underlying buffer item-by-item will do the job, but will
        // keep the header as-is, which is fine since the header does not contain secrets.
        for e in self.a_L.iter_mut() {
            e.clear();
        }
        for e in self.a_R.iter_mut() {
            e.clear();
        }
        for e in self.a_O.iter_mut() {
            e.clear();
        }
        // XXX use ClearOnDrop instead of doing the above
    }
}

impl<'a, 'b> ConstraintSystem for Prover<'a, 'b> {
    type RandomizedCS = RandomizingProver<'a, 'b>;

    fn multiply(
        &mut self,
        mut left: LinearCombination,
        mut right: LinearCombination,
    ) -> (Variable, Variable, Variable) {
        let (l_var, r_var, o_var) = self.allocate(|eval| {
            // Synthesize the assignments for l,r,o
            let l = eval(&left);
            let r = eval(&right);
            let o = l * r;
            Ok((l, r, o))
        })
            .expect("assignment function never fails");

       // Constrain l,r,o:
        left.terms.push((l_var, -Scalar::one()));
        right.terms.push((r_var, -Scalar::one()));
        self.constrain(left);
        self.constrain(right);

        (l_var, r_var, o_var)
    }

    fn allocate<F>(&mut self, assign_fn: F) -> Result<(Variable, Variable, Variable), R1CSError>
    where
        F: FnOnce(&dyn Fn(&LinearCombination) -> Scalar)
            -> Result<(Scalar, Scalar, Scalar), R1CSError>,
    {
        let (l, r, o) = {
            // The explicit type hint seems to be necessary to compile.
            let eval: &dyn for<'c> Fn(&'c LinearCombination) -> Scalar =
                &(|lc| self.eval(lc));
            assign_fn(eval)?
        };

        // Create variables for l,r,o ...
        let l_var = Variable::MultiplierLeft(self.a_L.len());
        let r_var = Variable::MultiplierRight(self.a_R.len());
        let o_var = Variable::MultiplierOutput(self.a_O.len());
        // ... and assign them
        self.a_L.push(l);
        self.a_R.push(r);
        self.a_O.push(o);

        Ok((l_var, r_var, o_var))
    }

    fn constrain(&mut self, lc: LinearCombination) {
        // TODO: check that the linear combinations are valid
        // (e.g. that variables are valid, that the linear combination evals to 0 for prover, etc).
        assert_eq!(self.eval(&lc), Scalar::zero());
        self.constraints.push(lc);
    }

    fn specify_randomized_constraints<F>(&mut self, callback: F) -> Result<(), R1CSError>
    where
        F: 'static + Fn(&mut Self::RandomizedCS) -> Result<(), R1CSError>,
    {
        self.deferred_constraints.push(Box::new(callback));
        Ok(())
    }
}

impl<'a, 'b> ConstraintSystem for RandomizingProver<'a, 'b> {
    type RandomizedCS = Self;

    fn multiply(
        &mut self,
        left: LinearCombination,
        right: LinearCombination,
    ) -> (Variable, Variable, Variable) {
        self.prover.multiply(left, right)
    }

    fn allocate<F>(&mut self, assign_fn: F) -> Result<(Variable, Variable, Variable), R1CSError>
    where
        F: FnOnce(&dyn Fn(&LinearCombination) -> Scalar)
            -> Result<(Scalar, Scalar, Scalar), R1CSError>,
    {
        self.prover.allocate(assign_fn)
    }

    fn constrain(&mut self, lc: LinearCombination) {
        self.prover.constrain(lc)
    }

    fn specify_randomized_constraints<F>(&mut self, callback: F) -> Result<(), R1CSError>
    where
        F: 'static + Fn(&mut Self::RandomizedCS) -> Result<(), R1CSError>,
    {
        callback(self)
    }
}

impl<'a, 'b> RandomizedConstraintSystem for RandomizingProver<'a, 'b> {
    fn challenge_scalar(&mut self, label: &'static [u8]) -> Scalar {
        self.prover.transcript.challenge_scalar(label)
    }
}

impl<'a, 'b> Prover<'a, 'b> {
    /// Construct an empty constraint system with specified external
    /// input variables.
    ///
    /// # Inputs
    ///
    /// The `bp_gens` and `pc_gens` are generators for Bulletproofs
    /// and for the Pedersen commitments, respectively.  The
    /// [`BulletproofGens`] should have `gens_capacity` greater than
    /// the number of multiplication constraints that will eventually
    /// be added into the constraint system.
    ///
    /// The `transcript` parameter is a Merlin proof transcript.  The
    /// `ProverCS` holds onto the `&mut Transcript` until it consumes
    /// itself during [`ProverCS::prove`], releasing its borrow of the
    /// transcript.  This ensures that the transcript cannot be
    /// altered except by the `ProverCS` before proving is complete.
    ///
    /// # Returns
    ///
    /// Returns a new `Prover` instance.
    pub fn new(
        bp_gens: &'b BulletproofGens,
        pc_gens: &'b PedersenGens,
        transcript: &'a mut Transcript,
    ) -> Self {
        transcript.r1cs_domain_sep();

        Prover {
            pc_gens,
            bp_gens,
            transcript,
            v: Vec::new(),
            v_blinding: Vec::new(),
            constraints: Vec::new(),
            a_L: Vec::new(),
            a_R: Vec::new(),
            a_O: Vec::new(),
            deferred_constraints: Vec::new(),
        }
    }

    /// Creates commitment to a high-level variable and adds it to the transcript.
    ///
    /// # Inputs
    ///
    /// The `v` and `v_blinding` parameters are openings to the
    /// commitment to the external variable for the constraint
    /// system.  Passing the opening (the value together with the
    /// blinding factor) makes it possible to reference pre-existing
    /// commitments in the constraint system.  All external variables
    /// must be passed up-front, so that challenges produced by
    /// [`ConstraintSystem::challenge_scalar`] are bound to the
    /// external variables.
    ///
    /// # Returns
    ///
    /// Returns a pair of a Pedersen commitment (as a compressed Ristretto point),
    /// and a [`Variable`] corresponding to it, which can be used to form constraints.
    pub fn commit(&mut self, v: Scalar, v_blinding: Scalar) -> (CompressedRistretto, Variable) {
        let i = self.v.len();
        self.v.push(v);
        self.v_blinding.push(v_blinding);

        // Add the commitment to the transcript.
        let V = self.pc_gens.commit(v, v_blinding).compress();
        self.transcript.commit_point(b"V", &V);

        (V, Variable::Committed(i))
    }

    /// Use a challenge, `z`, to flatten the constraints in the
    /// constraint system into vectors used for proving and
    /// verification.
    ///
    /// # Output
    ///
    /// Returns a tuple of
    /// ```text
    /// (wL, wR, wO, wV)
    /// ```
    /// where `w{L,R,O}` is \\( z \cdot z^Q \cdot W_{L,R,O} \\).
    fn flattened_constraints(
        &mut self,
        z: &Scalar,
    ) -> (Vec<Scalar>, Vec<Scalar>, Vec<Scalar>, Vec<Scalar>) {
        let n = self.a_L.len();
        let m = self.v.len();

        let mut wL = vec![Scalar::zero(); n];
        let mut wR = vec![Scalar::zero(); n];
        let mut wO = vec![Scalar::zero(); n];
        let mut wV = vec![Scalar::zero(); m];

        let mut exp_z = *z;
        for lc in self.constraints.iter() {
            for (var, coeff) in &lc.terms {
                match var {
                    Variable::MultiplierLeft(i) => {
                        wL[*i] += exp_z * coeff;
                    }
                    Variable::MultiplierRight(i) => {
                        wR[*i] += exp_z * coeff;
                    }
                    Variable::MultiplierOutput(i) => {
                        wO[*i] += exp_z * coeff;
                    }
                    Variable::Committed(i) => {
                        wV[*i] -= exp_z * coeff;
                    }
                    Variable::One() => {
                        // The prover doesn't need to handle constant terms
                    }
                }
            }
            exp_z *= z;
        }

        (wL, wR, wO, wV)
    }

    fn eval(&self, lc: &LinearCombination) -> Scalar {
        lc.terms
            .iter()
            .map(|(var, coeff)| {
                coeff
                    * match var {
                        Variable::MultiplierLeft(i) => self.a_L[*i],
                        Variable::MultiplierRight(i) => self.a_R[*i],
                        Variable::MultiplierOutput(i) => self.a_O[*i],
                        Variable::Committed(i) => self.v[*i],
                        Variable::One() => Scalar::one(),
                    }
            })
            .sum()
    }

    /// Calls all remembered callbacks with an API that
    /// allows generating challenge scalars.
    fn create_randomized_constraints(mut self) -> Result<Self, R1CSError> {
        // Note: the wrapper could've used &mut instead of ownership,
        // but specifying lifetimes for boxed closures is not going to be nice,
        // so we move the self into wrapper and then move it back out afterwards.
        let mut callbacks = mem::replace(&mut self.deferred_constraints, Vec::new());
        let mut wrapped_self = RandomizingProver { prover: self };
        for callback in callbacks.drain(..) {
            callback(&mut wrapped_self)?;
        }
        Ok(wrapped_self.prover)
    }

    /// Consume this `ConstraintSystem` to produce a proof.
    pub fn prove(mut self) -> Result<R1CSProof, R1CSError> {
        use std::iter;
        use util;

        // Commit a length _suffix_ for the number of high-level variables.
        // We cannot do this in advance because user can commit variables one-by-one,
        // but this suffix provides safe disambiguation because each variable
        // is prefixed with a separate label.
        self.transcript.commit_u64(b"m", self.v.len() as u64);

        // Create a `TranscriptRng` from the high-level witness data
        //
        // The prover wants to rekey the RNG with its witness data.
        //
        // This consists of the high level witness data (the v's and
        // v_blinding's), as well as the low-level witness data (a_L,
        // a_R, a_O).  Since the low-level data should (hopefully) be
        // determined by the high-level data, it doesn't give any
        // extra entropy for reseeding the RNG.
        //
        // Since the v_blindings should be random scalars (in order to
        // protect the v's in the commitments), we don't gain much by
        // committing the v's as well as the v_blinding's.
        let mut rng = {
            let mut builder = self.transcript.build_rng();

            // Commit the blinding factors for the input wires
            for v_b in &self.v_blinding {
                builder = builder.commit_witness_bytes(b"v_blinding", v_b.as_bytes());
            }

            use rand::thread_rng;
            builder.finalize(&mut thread_rng())
        };

        // Commit to the first-phase low-level witness variables.
        let n1 = self.a_L.len();

        if self.bp_gens.gens_capacity < n1 {
            return Err(R1CSError::InvalidGeneratorsLength);
        }

        // We are performing a single-party circuit proof, so party index is 0.
        let gens = self.bp_gens.share(0);

        let i_blinding1 = Scalar::random(&mut rng);
        let o_blinding1 = Scalar::random(&mut rng);
        let s_blinding1 = Scalar::random(&mut rng);

        let mut s_L1: Vec<Scalar> = (0..n1).map(|_| Scalar::random(&mut rng)).collect();
        let mut s_R1: Vec<Scalar> = (0..n1).map(|_| Scalar::random(&mut rng)).collect();

        // A_I = <a_L, G> + <a_R, H> + i_blinding * B_blinding
        let A_I1 = RistrettoPoint::multiscalar_mul(
            iter::once(&i_blinding1)
                .chain(self.a_L.iter())
                .chain(self.a_R.iter()),
            iter::once(&self.pc_gens.B_blinding)
                .chain(gens.G(n1))
                .chain(gens.H(n1)),
        )
        .compress();

        // A_O = <a_O, G> + o_blinding * B_blinding
        let A_O1 = RistrettoPoint::multiscalar_mul(
            iter::once(&o_blinding1).chain(self.a_O.iter()),
            iter::once(&self.pc_gens.B_blinding).chain(gens.G(n1)),
        )
        .compress();

        // S = <s_L, G> + <s_R, H> + s_blinding * B_blinding
        let S1 = RistrettoPoint::multiscalar_mul(
            iter::once(&s_blinding1)
                .chain(s_L1.iter())
                .chain(s_R1.iter()),
            iter::once(&self.pc_gens.B_blinding)
                .chain(gens.G(n1))
                .chain(gens.H(n1)),
        )
        .compress();

        self.transcript.commit_point(b"A_I1", &A_I1);
        self.transcript.commit_point(b"A_O1", &A_O1);
        self.transcript.commit_point(b"S1", &S1);

        // Process the remaining constraints.
        self = self.create_randomized_constraints()?;

        // Pad zeros to the next power of two (or do that implicitly when creating vectors)

        // If the number of multiplications is not 0 or a power of 2, then pad the circuit.
        let n = self.a_L.len();
        let n2 = n - n1;
        let padded_n = self.a_L.len().next_power_of_two();
        let pad = padded_n - n;

        if self.bp_gens.gens_capacity < padded_n {
            return Err(R1CSError::InvalidGeneratorsLength);
        }

        // Commit to the second-phase low-level witness variables

        let i_blinding2 = Scalar::random(&mut rng);
        let o_blinding2 = Scalar::random(&mut rng);
        let s_blinding2 = Scalar::random(&mut rng);

        let mut s_L2: Vec<Scalar> = (0..n2).map(|_| Scalar::random(&mut rng)).collect();
        let mut s_R2: Vec<Scalar> = (0..n2).map(|_| Scalar::random(&mut rng)).collect();

        // A_I = <a_L, G> + <a_R, H> + i_blinding * B_blinding
        let A_I2 = RistrettoPoint::multiscalar_mul(
            iter::once(&i_blinding2)
                .chain(self.a_L.iter().skip(n1))
                .chain(self.a_R.iter().skip(n1)),
            iter::once(&self.pc_gens.B_blinding)
                .chain(gens.G(n).skip(n1))
                .chain(gens.H(n).skip(n1)),
        )
        .compress();

        // A_O = <a_O, G> + o_blinding * B_blinding
        let A_O2 = RistrettoPoint::multiscalar_mul(
            iter::once(&o_blinding2).chain(self.a_O.iter().skip(n1)),
            iter::once(&self.pc_gens.B_blinding).chain(gens.G(n).skip(n1)),
        )
        .compress();

        // S = <s_L, G> + <s_R, H> + s_blinding * B_blinding
        let S2 = RistrettoPoint::multiscalar_mul(
            iter::once(&s_blinding2)
                .chain(s_L2.iter())
                .chain(s_R2.iter()),
            iter::once(&self.pc_gens.B_blinding)
                .chain(gens.G(n).skip(n1))
                .chain(gens.H(n).skip(n1)),
        )
        .compress();

        self.transcript.commit_point(b"A_I2", &A_I2);
        self.transcript.commit_point(b"A_O2", &A_O2);
        self.transcript.commit_point(b"S2", &S2);

        // 4. Compute blinded vector polynomials l(x) and r(x)

        let y = self.transcript.challenge_scalar(b"y");
        let z = self.transcript.challenge_scalar(b"z");

        let (wL, wR, wO, wV) = self.flattened_constraints(&z);

        let mut l_poly = util::VecPoly3::zero(n);
        let mut r_poly = util::VecPoly3::zero(n);

        let mut exp_y = Scalar::one(); // y^n starting at n=0
        let y_inv = y.invert();
        let exp_y_inv = util::exp_iter(y_inv).take(padded_n).collect::<Vec<_>>();

        let sLsR = s_L1
            .iter()
            .chain(s_L2.iter())
            .zip(s_R1.iter().chain(s_R2.iter()));
        for (i, (sl, sr)) in sLsR.enumerate() {
            // l_poly.0 = 0
            // l_poly.1 = a_L + y^-n * (z * z^Q * W_R)
            l_poly.1[i] = self.a_L[i] + exp_y_inv[i] * wR[i];
            // l_poly.2 = a_O
            l_poly.2[i] = self.a_O[i];
            // l_poly.3 = s_L
            l_poly.3[i] = *sl;
            // r_poly.0 = (z * z^Q * W_O) - y^n
            r_poly.0[i] = wO[i] - exp_y;
            // r_poly.1 = y^n * a_R + (z * z^Q * W_L)
            r_poly.1[i] = exp_y * self.a_R[i] + wL[i];
            // r_poly.2 = 0
            // r_poly.3 = y^n * s_R
            r_poly.3[i] = exp_y * sr;

            exp_y = exp_y * y; // y^i -> y^(i+1)
        }

        let t_poly = util::VecPoly3::special_inner_product(&l_poly, &r_poly);

        let t_1_blinding = Scalar::random(&mut rng);
        let t_3_blinding = Scalar::random(&mut rng);
        let t_4_blinding = Scalar::random(&mut rng);
        let t_5_blinding = Scalar::random(&mut rng);
        let t_6_blinding = Scalar::random(&mut rng);

        let T_1 = self.pc_gens.commit(t_poly.t1, t_1_blinding).compress();
        let T_3 = self.pc_gens.commit(t_poly.t3, t_3_blinding).compress();
        let T_4 = self.pc_gens.commit(t_poly.t4, t_4_blinding).compress();
        let T_5 = self.pc_gens.commit(t_poly.t5, t_5_blinding).compress();
        let T_6 = self.pc_gens.commit(t_poly.t6, t_6_blinding).compress();

        self.transcript.commit_point(b"T_1", &T_1);
        self.transcript.commit_point(b"T_3", &T_3);
        self.transcript.commit_point(b"T_4", &T_4);
        self.transcript.commit_point(b"T_5", &T_5);
        self.transcript.commit_point(b"T_6", &T_6);

        let u = self.transcript.challenge_scalar(b"u");
        let x = self.transcript.challenge_scalar(b"x");

        // t_2_blinding = <z*z^Q, W_V * v_blinding>
        // in the t_x_blinding calculations, line 76.
        let t_2_blinding = wV
            .iter()
            .zip(self.v_blinding.iter())
            .map(|(c, v_blinding)| c * v_blinding)
            .sum();

        let t_blinding_poly = util::Poly6 {
            t1: t_1_blinding,
            t2: t_2_blinding,
            t3: t_3_blinding,
            t4: t_4_blinding,
            t5: t_5_blinding,
            t6: t_6_blinding,
        };

        let t_x = t_poly.eval(x);
        let t_x_blinding = t_blinding_poly.eval(x);
        let mut l_vec = l_poly.eval(x);
        l_vec.append(&mut vec![Scalar::zero(); pad]);

        let mut r_vec = r_poly.eval(x);
        r_vec.append(&mut vec![Scalar::zero(); pad]);

        // XXX this should refer to the notes to explain why this is correct
        for i in n..padded_n {
            r_vec[i] = -exp_y;
            exp_y = exp_y * y; // y^i -> y^(i+1)
        }

        let i_blinding = i_blinding1 + u * i_blinding2;
        let o_blinding = o_blinding1 + u * o_blinding2;
        let s_blinding = s_blinding1 + u * s_blinding2;

        let e_blinding = x * (i_blinding + x * (o_blinding + x * s_blinding));

        self.transcript.commit_scalar(b"t_x", &t_x);
        self.transcript
            .commit_scalar(b"t_x_blinding", &t_x_blinding);
        self.transcript.commit_scalar(b"e_blinding", &e_blinding);

        // Get a challenge value to combine statements for the IPP
        let w = self.transcript.challenge_scalar(b"w");
        let Q = w * self.pc_gens.B;

        let G_factors = iter::repeat(Scalar::one())
            .take(n1)
            .chain(iter::repeat(u).take(n2 + pad))
            .collect::<Vec<_>>();
        let H_factors = exp_y_inv
            .into_iter()
            .zip(G_factors.iter())
            .map(|(y, u_or_1)| y * u_or_1)
            .collect::<Vec<_>>();

        let ipp_proof = InnerProductProof::create(
            self.transcript,
            &Q,
            &G_factors,
            &H_factors,
            gens.G(padded_n).cloned().collect(),
            gens.H(padded_n).cloned().collect(),
            l_vec,
            r_vec,
        );

        // We do not yet have a ClearOnDrop wrapper for Vec<Scalar>.
        // When PR 202 [1] is merged, we can simply wrap s_L and s_R at the point of creation.
        // [1] https://github.com/dalek-cryptography/curve25519-dalek/pull/202
        for scalar in s_L1
            .iter_mut()
            .chain(s_L2.iter_mut())
            .chain(s_R1.iter_mut())
            .chain(s_R2.iter_mut())
        {
            scalar.clear();
        }

        Ok(R1CSProof {
            A_I1,
            A_O1,
            S1,
            A_I2,
            A_O2,
            S2,
            T_1,
            T_3,
            T_4,
            T_5,
            T_6,
            t_x,
            t_x_blinding,
            e_blinding,
            ipp_proof,
        })
    }
}
