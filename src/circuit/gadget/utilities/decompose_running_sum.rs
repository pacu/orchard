//! Decomposes an $n$-bit field element $\alpha$ into $W$ windows, each window
//! being a $K$-bit word, using a running sum $z$.
//! We constrain $K \leq 3$ for this helper.
//!     $$\alpha = k_0 + (2^K) k_1 + (2^{2K}) k_2 + ... + (2^{(W-1)K}) k_{W-1}$$
//!
//! $z_0$ is initialized as $\alpha$. Each successive $z_{i+1}$ is computed as
//!                $$z_{i+1} = (z_{i} - k_i) / (2^K).$$
//! $z_W$ is constrained to be zero.
//! The difference between each interstitial running sum output is constrained
//! to be $K$ bits, i.e.
//!                      `range_check`($k_i$, $2^K$),
//! where
//! ```text
//!   range_check(word, range)
//!     = word * (1 - word) * (2 - word) * ... * ((range - 1) - word)
//! ```
//!
//! Given that the `range_check` constraint will be toggled by a selector, in
//! practice we will have a `selector * range_check(word, range)` expression
//! of degree `range + 1`.
//!
//! This means that $2^K$ has to be at most `degree_bound - 1` in order for
//! the range check constraint to stay within the degree bound.

use ff::PrimeFieldBits;
use halo2::{
    circuit::Region,
    plonk::{Advice, Column, ConstraintSystem, Error, Permutation, Selector},
    poly::Rotation,
};

use super::{copy, range_check, CellValue, Var};
use crate::constants::util::decompose_word;
use pasta_curves::arithmetic::FieldExt;
use std::marker::PhantomData;

/// The running sum $[z_1, ..., z_W]$. If created in strict mode, $z_W = 0$.
pub struct RunningSum<F: FieldExt + PrimeFieldBits>(Vec<CellValue<F>>);
impl<F: FieldExt + PrimeFieldBits> std::ops::Deref for RunningSum<F> {
    type Target = Vec<CellValue<F>>;

    fn deref(&self) -> &Vec<CellValue<F>> {
        &self.0
    }
}
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RunningSumConfig<F: FieldExt + PrimeFieldBits, const WINDOW_NUM_BITS: usize> {
    q_range_check: Selector,
    q_strict: Selector,
    pub z: Column<Advice>,
    perm: Permutation,
    _marker: PhantomData<F>,
}

impl<F: FieldExt + PrimeFieldBits, const WINDOW_NUM_BITS: usize>
    RunningSumConfig<F, WINDOW_NUM_BITS>
{
    /// `perm` MUST include the advice column `z`.
    ///
    /// # Panics
    ///
    /// Panics if WINDOW_NUM_BITS > 3.
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        q_range_check: Selector,
        z: Column<Advice>,
        perm: Permutation,
    ) -> Self {
        assert!(WINDOW_NUM_BITS <= 3);

        let config = Self {
            q_range_check,
            q_strict: meta.selector(),
            z,
            perm,
            _marker: PhantomData,
        };

        meta.create_gate("range check", |meta| {
            let q_range_check = meta.query_selector(config.q_range_check);
            let z_cur = meta.query_advice(config.z, Rotation::cur());
            let z_next = meta.query_advice(config.z, Rotation::next());
            //    z_i = 2^{K}⋅z_{i + 1} + k_i
            // => k_i = z_i - 2^{K}⋅z_{i + 1}
            let word = z_cur - z_next * F::from_u64(1 << WINDOW_NUM_BITS);

            vec![q_range_check * range_check(word, 1 << WINDOW_NUM_BITS)]
        });

        meta.create_gate("final z = 0", |meta| {
            let q_strict = meta.query_selector(config.q_strict);
            let z_final = meta.query_advice(config.z, Rotation::cur());

            vec![q_strict * z_final]
        });

        config
    }

    /// Decompose a field element alpha that is witnessed in this helper.
    ///
    /// `strict` = true constrains the final running sum to be zero, i.e.
    /// constrains alpha to be within WINDOW_NUM_BITS * num_windows bits.
    pub fn witness_decompose(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        alpha: Option<F>,
        strict: bool,
        word_num_bits: usize,
        num_windows: usize,
    ) -> Result<(CellValue<F>, RunningSum<F>), Error> {
        let z_0 = {
            let cell = region.assign_advice(
                || "z_0 = alpha",
                self.z,
                offset,
                || alpha.ok_or(Error::SynthesisError),
            )?;
            CellValue::new(cell, alpha)
        };
        self.decompose(region, offset, z_0, strict, word_num_bits, num_windows)
    }

    /// Decompose an existing variable alpha that is copied into this helper.
    ///
    /// `strict` = true constrains the final running sum to be zero, i.e.
    /// constrains alpha to be within WINDOW_NUM_BITS * num_windows bits.
    pub fn copy_decompose(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        alpha: CellValue<F>,
        strict: bool,
        word_num_bits: usize,
        num_windows: usize,
    ) -> Result<(CellValue<F>, RunningSum<F>), Error> {
        let z_0 = copy(
            region,
            || "copy z_0 = alpha",
            self.z,
            offset,
            &alpha,
            &self.perm,
        )?;
        self.decompose(region, offset, z_0, strict, word_num_bits, num_windows)
    }

    /// `z_0` must be the cell at `(self.z, offset)` in `region`.
    ///
    /// # Panics
    ///
    /// Panics if there are too many windows for the given word size.
    fn decompose(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        z_0: CellValue<F>,
        strict: bool,
        word_num_bits: usize,
        num_windows: usize,
    ) -> Result<(CellValue<F>, RunningSum<F>), Error> {
        // Make sure that we do not have more windows than required for the number
        // of bits in the word. In other words, every window must contain at least
        // one bit of the word (no empty windows).
        //
        // For example, let:
        //      - word_num_bits = 64
        //      - WINDOW_NUM_BITS = 3
        // In this case, the maximum allowed num_windows is 22:
        //                    3 * 22 < 64 + 3
        //
        assert!(WINDOW_NUM_BITS * num_windows < word_num_bits + WINDOW_NUM_BITS);

        // Enable selectors
        {
            for idx in 0..num_windows {
                self.q_range_check.enable(region, offset + idx)?;
            }

            if strict {
                // Constrain the final running sum output to be zero.
                self.q_strict.enable(region, offset + num_windows)?;
            }
        }

        // Decompose base field element into K-bit words.
        let words: Vec<Option<u8>> = {
            let words = z_0
                .value()
                .map(|word| decompose_word::<F>(word, word_num_bits, WINDOW_NUM_BITS));

            if let Some(words) = words {
                words.into_iter().map(Some).collect()
            } else {
                vec![None; num_windows]
            }
        };

        // Initialize empty vector to store running sum values [z_1, ..., z_W].
        let mut zs: Vec<CellValue<F>> = Vec::with_capacity(num_windows);
        let mut z = z_0;

        // Assign running sum `z_{i+1}` = (z_i - k_i) / (2^K) for i = 0..=n-1.
        // Outside of this helper, z_0 = alpha must have already been loaded into the
        // `z` column at `offset`.
        let two_pow_k_inv = F::from_u64(1 << WINDOW_NUM_BITS as u64).invert().unwrap();
        for (i, word) in words.iter().enumerate() {
            // z_next = (z_cur - word) / (2^K)
            let z_next = {
                let word = word.map(|word| F::from_u64(word as u64));
                let z_next_val = z
                    .value()
                    .zip(word)
                    .map(|(z_cur_val, word)| (z_cur_val - word) * two_pow_k_inv);
                let cell = region.assign_advice(
                    || format!("z_{:?}", i + 1),
                    self.z,
                    offset + i + 1,
                    || z_next_val.ok_or(Error::SynthesisError),
                )?;
                CellValue::new(cell, z_next_val)
            };

            // Update `z`.
            z = z_next;
            zs.push(z);
        }

        Ok((z_0, RunningSum(zs)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        FIXED_BASE_WINDOW_SIZE, L_ORCHARD_BASE, L_VALUE, NUM_WINDOWS, NUM_WINDOWS_SHORT,
    };
    use halo2::{
        circuit::{Layouter, SimpleFloorPlanner},
        dev::{MockProver, VerifyFailure},
        plonk::{Circuit, ConstraintSystem, Error},
    };
    use pasta_curves::{arithmetic::FieldExt, pallas};

    #[test]
    fn test_running_sum() {
        struct MyCircuit<
            F: FieldExt + PrimeFieldBits,
            const WORD_NUM_BITS: usize,
            const WINDOW_NUM_BITS: usize,
            const NUM_WINDOWS: usize,
        > {
            alpha: Option<F>,
            strict: bool,
        }

        impl<
                F: FieldExt + PrimeFieldBits,
                const WORD_NUM_BITS: usize,
                const WINDOW_NUM_BITS: usize,
                const NUM_WINDOWS: usize,
            > Circuit<F> for MyCircuit<F, WORD_NUM_BITS, WINDOW_NUM_BITS, NUM_WINDOWS>
        {
            type Config = RunningSumConfig<F, WINDOW_NUM_BITS>;
            type FloorPlanner = SimpleFloorPlanner;

            fn without_witnesses(&self) -> Self {
                Self {
                    alpha: None,
                    strict: self.strict,
                }
            }

            fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
                let z = meta.advice_column();
                let q_range_check = meta.selector();
                let perm = meta.permutation(&[z.into()]);

                RunningSumConfig::<F, WINDOW_NUM_BITS>::configure(meta, q_range_check, z, perm)
            }

            fn synthesize(
                &self,
                config: Self::Config,
                mut layouter: impl Layouter<F>,
            ) -> Result<(), Error> {
                layouter.assign_region(
                    || "decompose",
                    |mut region| {
                        let offset = 0;
                        let (alpha, _zs) = config.witness_decompose(
                            &mut region,
                            offset,
                            self.alpha,
                            self.strict,
                            WORD_NUM_BITS,
                            NUM_WINDOWS,
                        )?;

                        let offset = offset + NUM_WINDOWS + 1;

                        config.copy_decompose(
                            &mut region,
                            offset,
                            alpha,
                            self.strict,
                            WORD_NUM_BITS,
                            NUM_WINDOWS,
                        )?;

                        Ok(())
                    },
                )
            }
        }

        // Random base field element
        {
            let alpha = pallas::Base::rand();

            // Strict full decomposition should pass.
            let circuit: MyCircuit<
                pallas::Base,
                L_ORCHARD_BASE,
                FIXED_BASE_WINDOW_SIZE,
                NUM_WINDOWS,
            > = MyCircuit {
                alpha: Some(alpha),
                strict: true,
            };
            let prover = MockProver::<pallas::Base>::run(8, &circuit, vec![]).unwrap();
            assert_eq!(prover.verify(), Ok(()));
        }

        // Random 64-bit word
        {
            let alpha = pallas::Base::from_u64(rand::random());

            // Strict full decomposition should pass.
            let circuit: MyCircuit<
                pallas::Base,
                L_VALUE,
                FIXED_BASE_WINDOW_SIZE,
                NUM_WINDOWS_SHORT,
            > = MyCircuit {
                alpha: Some(alpha),
                strict: true,
            };
            let prover = MockProver::<pallas::Base>::run(8, &circuit, vec![]).unwrap();
            assert_eq!(prover.verify(), Ok(()));
        }

        // 2^66
        {
            let alpha = pallas::Base::from_u128(1 << 66);

            // Strict partial decomposition should fail.
            let circuit: MyCircuit<
                pallas::Base,
                L_ORCHARD_BASE,
                FIXED_BASE_WINDOW_SIZE,
                NUM_WINDOWS_SHORT,
            > = MyCircuit {
                alpha: Some(alpha),
                strict: true,
            };
            let prover = MockProver::<pallas::Base>::run(8, &circuit, vec![]).unwrap();
            assert_eq!(
                prover.verify(),
                Err(vec![
                    VerifyFailure::Constraint {
                        constraint: ((1, "final z = 0").into(), 0, "").into(),
                        row: 22
                    },
                    VerifyFailure::Constraint {
                        constraint: ((1, "final z = 0").into(), 0, "").into(),
                        row: 45
                    }
                ])
            );

            // Non-strict partial decomposition should pass.
            let circuit: MyCircuit<
                pallas::Base,
                L_ORCHARD_BASE,
                FIXED_BASE_WINDOW_SIZE,
                NUM_WINDOWS_SHORT,
            > = MyCircuit {
                alpha: Some(alpha),
                strict: false,
            };
            let prover = MockProver::<pallas::Base>::run(8, &circuit, vec![]).unwrap();
            assert_eq!(prover.verify(), Ok(()));
        }
    }
}