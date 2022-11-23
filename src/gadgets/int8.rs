use anyhow::{anyhow, bail, Result};
use ark_ff::{Field, One, PrimeField, Zero};
use ark_r1cs_std::{
    boolean::AllocatedBool,
    prelude::{AllocVar, AllocationMode, Boolean, EqGadget},
    Assignment, R1CSVar, ToBitsGadget,
};
use ark_relations::{
    lc,
    r1cs::{ConstraintSystemRef, LinearCombination, Namespace, SynthesisError, Variable},
};
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::cast::ToPrimitive;
use std::ops::Add;
use std::{borrow::Borrow, ops::Sub};

const I8_SIZE_IN_BITS: usize = 8;
const OPERANDS_LEN: usize = 2;

/// Represents an interpretation of 8 `Boolean` objects as an
/// unsigned integer.
#[derive(Clone, Debug)]
pub struct Int8<F: Field> {
    /// Little-endian representation: least significant bit first
    pub(crate) bits: [Boolean<F>; 8],
    pub(crate) value: Option<i8>,
}

impl<F: Field> Int8<F> {
    /// Construct a constant `UInt8` from a `u8`
    ///
    /// This *does not* create new variables or constraints.
    ///
    /// ```
    /// # fn main() -> Result<(), ark_relations::r1cs::SynthesisError> {
    /// // We'll use the BLS12-381 scalar field for our constraints.
    /// use simpleworks::gadgets::int8::Int8;
    /// use ark_bls12_381::Fr;
    /// use ark_relations::r1cs::*;
    /// use ark_r1cs_std::prelude::*;
    ///
    /// let cs = ConstraintSystem::<Fr>::new_ref();
    /// let var = Int8::new_witness(cs.clone(), || Ok(2))?;
    ///
    /// let constant = Int8::constant(2);
    /// var.enforce_equal(&constant)?;
    /// assert!(cs.is_satisfied().unwrap());
    /// # Ok(())
    /// # }
    /// ```
    pub fn constant(value: i8) -> Self {
        let mut bits = [Boolean::FALSE; 8];

        let mut tmp = value;

        bits.iter_mut().for_each(|bit| {
            // If last bit is one, push one.
            *bit = Boolean::constant((tmp & 1) == 1);
            tmp >>= 1_i32;
        });

        Self {
            bits,
            value: Some(value),
        }
    }

    /// Perform modular addition of `operands`.
    ///
    /// The user must ensure that overflow does not occur.
    pub fn addmany(operands: &[Self; OPERANDS_LEN]) -> Result<Self>
    where
        F: PrimeField,
    {
        // Compute the maximum value of the sum so we allocate enough bits for
        // the result
        let mut max_value = BigInt::from(i8::max_value()) * BigInt::from(OPERANDS_LEN);

        // Keep track of the resulting value
        let mut result_value = Some(BigInt::zero());

        // This is a linear combination that we will enforce to be "zero"
        let mut lc = LinearCombination::zero();

        let mut all_constants = true;

        // Iterate over the operands
        for op in operands {
            // Accumulate the value
            match op.value {
                Some(val) => {
                    if let Some(v) = result_value.as_mut() {
                        *v += BigInt::from(val)
                    }
                }

                None => {
                    // If any of our operands have unknown value, we won't
                    // know the value of the result
                    result_value = None;
                }
            }

            // Iterate over each bit_gadget of the operand and add the operand to
            // the linear combination
            let mut coeff = F::one();
            for bit in &op.bits {
                match *bit {
                    Boolean::Is(ref bit) => {
                        all_constants = false;

                        // Add coeff * bit_gadget
                        lc += (coeff, bit.variable());
                    }
                    Boolean::Not(ref bit) => {
                        all_constants = false;

                        // Add coeff * (1 - bit_gadget) = coeff * ONE - coeff * bit_gadget
                        lc = lc + (coeff, Variable::One) - (coeff, bit.variable());
                    }
                    Boolean::Constant(bit) => {
                        if bit {
                            lc += (coeff, Variable::One);
                        }
                    }
                }

                coeff.double_in_place();
            }
        }

        // The value of the actual result is modulo 2^$size
        let modular_value = result_value.clone().map(|v| {
            let modulus = BigInt::from(1_u64)
                << (I8_SIZE_IN_BITS
                    .to_u32()
                    .ok_or("I8_SIZE_IN_BITS value cannot be represented as u32.")?);

            let shift = BigInt::from(1_u64)
                << ((I8_SIZE_IN_BITS - 1)
                    .to_u32()
                    .ok_or("I8_SIZE_IN_BITS value cannot be represented as u32.")?);

            (v.add(shift.clone()).mod_floor(&modulus))
                .sub(shift)
                .to_i8()
                .ok_or("Modular value cannot be represented as i8.")
        });

        if let Some(Ok(modular_value)) = modular_value {
            if all_constants {
                return Ok(Self::constant(modular_value));
            }
        }
        let cs = operands.cs();

        // Storage area for the resulting bits
        let mut result_bits = vec![];

        // Allocate each bit_gadget of the result
        let mut coeff = F::one();
        let mut i = 0_i32;
        while max_value != BigInt::zero() {
            // Allocate the bit_gadget
            let b = AllocatedBool::new_witness(cs.clone(), || {
                result_value
                    .clone()
                    .map(|v| (v >> i) & BigInt::one() == BigInt::one())
                    .get()
            })?;

            // Subtract this bit_gadget from the linear combination to ensure the sums
            // balance out
            lc = lc - (coeff, b.variable());

            result_bits.push(b.into());

            max_value >>= 1_i32;
            i += 1_i32;
            coeff.double_in_place();
        }

        // Enforce that the linear combination equals zero
        cs.enforce_constraint(lc!(), lc!(), lc)?;

        // Discard carry bits that we don't care about
        result_bits.truncate(I8_SIZE_IN_BITS);
        let bits = TryFrom::try_from(result_bits).map_err(|e| anyhow!("{:?}", e))?;

        match modular_value {
            Some(Ok(modular_value)) => Ok(Self {
                bits,
                value: Some(modular_value),
            }),
            Some(Err(e)) => bail!("{e}"),
            None => bail!("The result of the modular addition between Int8 is None"),
        }
    }
}

impl<ConstraintF: Field> AllocVar<i8, ConstraintF> for Int8<ConstraintF> {
    fn new_variable<T: Borrow<i8>>(
        cs: impl Into<Namespace<ConstraintF>>,
        f: impl FnOnce() -> Result<T, SynthesisError>,
        mode: AllocationMode,
    ) -> Result<Self, SynthesisError> {
        let ns = cs.into();
        let cs = ns.cs();
        let value = f().map(|f| *f.borrow()).ok();

        let mut values = [None; I8_SIZE_IN_BITS];
        if let Some(val) = value {
            values
                .iter_mut()
                .enumerate()
                .for_each(|(i, v)| *v = Some((val >> i) & 1 == 1));
        }

        let mut bits = [Boolean::FALSE; I8_SIZE_IN_BITS];
        for (b, v) in bits.iter_mut().zip(&values) {
            *b = Boolean::new_variable(cs.clone(), || v.get(), mode)?;
        }
        Ok(Self { bits, value })
    }
}

impl<ConstraintF: Field> EqGadget<ConstraintF> for Int8<ConstraintF> {
    fn is_eq(&self, other: &Self) -> Result<Boolean<ConstraintF>, SynthesisError> {
        self.bits.as_ref().is_eq(&other.bits)
    }

    fn conditional_enforce_equal(
        &self,
        other: &Self,
        condition: &Boolean<ConstraintF>,
    ) -> Result<(), SynthesisError> {
        self.bits.conditional_enforce_equal(&other.bits, condition)
    }

    fn conditional_enforce_not_equal(
        &self,
        other: &Self,
        condition: &Boolean<ConstraintF>,
    ) -> Result<(), SynthesisError> {
        self.bits
            .conditional_enforce_not_equal(&other.bits, condition)
    }
}

impl<F: Field> ToBitsGadget<F> for Int8<F> {
    fn to_bits_le(&self) -> Result<Vec<Boolean<F>>, SynthesisError> {
        Ok(self.bits.to_vec())
    }
}

impl<F: Field> R1CSVar<F> for Int8<F> {
    type Value = i8;

    fn cs(&self) -> ConstraintSystemRef<F> {
        self.bits.as_ref().cs()
    }

    fn value(&self) -> Result<Self::Value, SynthesisError> {
        let mut value = None;
        for (i, bit) in self.bits.iter().enumerate() {
            let b = i8::from(bit.value()?);
            value = match value {
                Some(value) => Some(value + (b << i)),
                None => Some(b << i),
            };
        }
        debug_assert_eq!(self.value, value);
        value.get()
    }
}

#[cfg(test)]
mod tests {
    use super::Int8;
    use ark_bls12_381::Fr;
    use ark_r1cs_std::{
        prelude::{AllocVar, EqGadget},
        R1CSVar, ToBitsGadget,
    };
    use ark_relations::r1cs::ConstraintSystem;

    #[test]
    fn test_int8_from_bits_to_bits() {
        let cs = ConstraintSystem::<Fr>::new_ref();
        let byte_val = 0b01110001;
        let byte =
            Int8::new_witness(ark_relations::ns!(cs, "alloc value"), || Ok(byte_val)).unwrap();
        let bits = byte.to_bits_le().unwrap();

        for (i, bit) in bits.iter().enumerate() {
            assert_eq!(bit.value().unwrap(), (byte_val >> i) & 1 == 1)
        }
    }

    #[test]
    fn test_positive() {
        let cs = ConstraintSystem::<Fr>::new_ref();
        let primitive_value = 1;
        let primitive_other_value = 1;
        let value_var =
            Int8::new_witness(ark_relations::ns!(cs, "value_var"), || Ok(primitive_value)).unwrap();
        let other_value_var = Int8::new_witness(ark_relations::ns!(cs, "other_value_var"), || {
            Ok(primitive_other_value)
        })
        .unwrap();

        assert!(value_var.enforce_equal(&other_value_var).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_value, value_var.value().unwrap());
        assert_eq!(primitive_other_value, other_value_var.value().unwrap());
    }

    #[test]
    fn test_negative() {
        let cs = ConstraintSystem::<Fr>::new_ref();
        let primitive_xxx = -1;
        let primitive_yyy = -1;
        let xxx = Int8::new_witness(ark_relations::ns!(cs, "xxx"), || Ok(primitive_xxx)).unwrap();
        let yyy = Int8::new_witness(ark_relations::ns!(cs, "yyy"), || Ok(primitive_yyy)).unwrap();

        assert!(xxx.enforce_equal(&yyy).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_xxx, xxx.value().unwrap());
        assert_eq!(primitive_yyy, yyy.value().unwrap());
    }

    #[test]
    fn test_addition_with_positive_operands() {
        let cs = ConstraintSystem::<Fr>::new_ref();
        let primitive_addend = 1;
        let primitive_augend = 1;
        let primitive_result = primitive_addend + primitive_augend;
        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result_from_primitive_var =
            Int8::new_witness(ark_relations::ns!(cs, "result"), || Ok(primitive_result)).unwrap();
        let result = Int8::addmany(&[addend_var, augend_var]).unwrap();

        assert!(result_from_primitive_var.enforce_equal(&result).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_result, result.value().unwrap());
    }

    #[test]
    fn test_addition_with_negative_operands() {
        let cs = ConstraintSystem::<Fr>::new_ref();
        let primitive_addend = -1;
        let primitive_augend = -1;
        let primitive_result = primitive_addend + primitive_augend;
        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result_from_primitive_var =
            Int8::new_witness(ark_relations::ns!(cs, "result"), || Ok(primitive_result)).unwrap();
        let result = Int8::addmany(&[addend_var, augend_var]).unwrap();

        assert!(result_from_primitive_var.enforce_equal(&result).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_result, result.value().unwrap());
    }

    #[test]
    fn test_addition_with_positive_addend_negative_augend_positive_result() {
        let cs = ConstraintSystem::<Fr>::new_ref();

        let primitive_addend = 2;
        let primitive_augend = -3;
        let primitive_result = primitive_addend + primitive_augend;

        let result_from_primitive_var =
            Int8::new_witness(ark_relations::ns!(cs, "result"), || Ok(primitive_result)).unwrap();
        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result = Int8::addmany(&[addend_var, augend_var]).unwrap();

        assert!(result_from_primitive_var.enforce_equal(&result).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_result, result.value().unwrap());
    }

    #[test]
    fn test_addition_with_negative_addend_positive_augend_positive_result() {
        let cs = ConstraintSystem::<Fr>::new_ref();

        let primitive_addend = -1;
        let primitive_augend = 2;
        let primitive_result = primitive_addend + primitive_augend;

        let result_from_primitive_var =
            Int8::new_witness(ark_relations::ns!(cs, "result"), || Ok(primitive_result)).unwrap();
        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result = Int8::addmany(&[addend_var, augend_var]).unwrap();

        assert!(result_from_primitive_var.enforce_equal(&result).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_result, result.value().unwrap());
    }

    #[test]
    fn test_addition_with_positive_addend_negative_augend_negative_result() {
        let cs = ConstraintSystem::<Fr>::new_ref();

        let primitive_addend = 1;
        let primitive_augend = -2;
        let primitive_result = primitive_addend + primitive_augend;

        let result_from_primitive_var =
            Int8::new_witness(ark_relations::ns!(cs, "result"), || Ok(primitive_result)).unwrap();
        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result = Int8::addmany(&[addend_var, augend_var]).unwrap();

        assert!(result_from_primitive_var.enforce_equal(&result).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_result, result.value().unwrap());
    }

    #[test]
    fn test_addition_with_negative_addend_positive_augend_negative_result() {
        let cs = ConstraintSystem::<Fr>::new_ref();

        let primitive_addend = -2;
        let primitive_augend = 1;
        let primitive_result = primitive_addend + primitive_augend;

        let result_from_primitive_var =
            Int8::new_witness(ark_relations::ns!(cs, "result"), || Ok(primitive_result)).unwrap();
        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result = Int8::addmany(&[addend_var, augend_var]).unwrap();

        assert!(result_from_primitive_var.enforce_equal(&result).is_ok());
        assert!(cs.is_satisfied().unwrap());
        assert_eq!(primitive_result, result.value().unwrap());
    }

    #[test]
    fn test_addition_with_overflow() {
        let cs = ConstraintSystem::<Fr>::new_ref();

        let primitive_addend = i8::max_value();
        let primitive_augend = 1;

        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result = Int8::addmany(&[addend_var, augend_var]);

        assert!(result.is_err());
    }

    #[test]
    fn test_addition_with_underflow() {
        let cs = ConstraintSystem::<Fr>::new_ref();

        let primitive_addend = -i8::max_value();
        let primitive_augend = -2;

        let addend_var =
            Int8::new_witness(ark_relations::ns!(cs, "addend"), || Ok(primitive_addend)).unwrap();
        let augend_var =
            Int8::new_witness(ark_relations::ns!(cs, "augend"), || Ok(primitive_augend)).unwrap();

        let result = Int8::addmany(&[addend_var, augend_var]);

        assert!(result.is_err());
    }
}
