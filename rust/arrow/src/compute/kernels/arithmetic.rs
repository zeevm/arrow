// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Defines basic arithmetic kernels for `PrimitiveArrays`.
//!
//! These kernels can leverage SIMD if available on your system.  Currently no runtime
//! detection is provided, you should enable the specific SIMD intrinsics using
//! `RUSTFLAGS="-C target-feature=+avx2"` for example.  See the documentation
//! [here](https://doc.rust-lang.org/stable/core/arch/) for more information.

#[cfg(feature = "simd")]
use std::mem;
use std::ops::{Add, Div, Mul, Sub};
#[cfg(feature = "simd")]
use std::slice::from_raw_parts_mut;
use std::sync::Arc;

use num::{One, Zero};

#[cfg(feature = "simd")]
use crate::bitmap::Bitmap;
use crate::buffer::Buffer;
#[cfg(feature = "simd")]
use crate::buffer::MutableBuffer;
use crate::compute::util::combine_option_bitmap;
#[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
use crate::compute::util::simd_load_set_invalid;
use crate::datatypes;
use crate::datatypes::ToByteSlice;
use crate::error::{ArrowError, Result};
use crate::{array::*, util::bit_util};

/// Helper function to perform math lambda function on values from two arrays. If either
/// left or right value is null then the output value is also null, so `1 + null` is
/// `null`.
///
/// # Errors
///
/// This function errors if the arrays have different lengths
pub fn math_op<T, F>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
    op: F,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    F: Fn(T::Native, T::Native) -> T::Native,
{
    if left.len() != right.len() {
        return Err(ArrowError::ComputeError(
            "Cannot perform math operation on arrays of different length".to_string(),
        ));
    }

    let null_bit_buffer =
        combine_option_bitmap(left.data_ref(), right.data_ref(), left.len())?;

    let values = (0..left.len())
        .map(|i| op(left.value(i), right.value(i)))
        .collect::<Vec<T::Native>>();

    let data = ArrayData::new(
        T::DATA_TYPE,
        left.len(),
        None,
        null_bit_buffer,
        0,
        vec![Buffer::from(values.to_byte_slice())],
        vec![],
    );
    Ok(PrimitiveArray::<T>::from(Arc::new(data)))
}

/// Helper function to divide two arrays.
///
/// # Errors
///
/// This function errors if:
/// * the arrays have different lengths
/// * a division by zero is found
fn math_divide<T>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Native: Div<Output = T::Native> + Zero,
{
    if left.len() != right.len() {
        return Err(ArrowError::ComputeError(
            "Cannot perform math operation on arrays of different length".to_string(),
        ));
    }

    let null_bit_buffer =
        combine_option_bitmap(left.data_ref(), right.data_ref(), left.len())?;

    let mut values = Vec::with_capacity(left.len());
    if let Some(b) = &null_bit_buffer {
        // some value is null
        for i in 0..left.len() {
            values.push(unsafe {
                if bit_util::get_bit_raw(b.raw_data(), i) {
                    let right_value = right.value(i);
                    if right_value.is_zero() {
                        return Err(ArrowError::DivideByZero);
                    } else {
                        left.value(i) / right_value
                    }
                } else {
                    T::default_value()
                }
            });
        }
    } else {
        // no value is null
        for i in 0..left.len() {
            let right_value = right.value(i);
            values.push(if right_value.is_zero() {
                return Err(ArrowError::DivideByZero);
            } else {
                left.value(i) / right_value
            });
        }
    };

    let data = ArrayData::new(
        T::DATA_TYPE,
        left.len(),
        None,
        null_bit_buffer,
        0,
        vec![Buffer::from(values.to_byte_slice())],
        vec![],
    );
    Ok(PrimitiveArray::<T>::from(Arc::new(data)))
}

/// SIMD vectorized version of `math_op` above.
#[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
fn simd_math_op<T, F>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
    op: F,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Simd: Add<Output = T::Simd>
        + Sub<Output = T::Simd>
        + Mul<Output = T::Simd>
        + Div<Output = T::Simd>,
    F: Fn(T::Simd, T::Simd) -> T::Simd,
{
    if left.len() != right.len() {
        return Err(ArrowError::ComputeError(
            "Cannot perform math operation on arrays of different length".to_string(),
        ));
    }

    let null_bit_buffer =
        combine_option_bitmap(left.data_ref(), right.data_ref(), left.len())?;

    let lanes = T::lanes();
    let buffer_size = left.len() * mem::size_of::<T::Native>();
    let mut result = MutableBuffer::new(buffer_size).with_bitset(buffer_size, false);

    for i in (0..left.len()).step_by(lanes) {
        let simd_left = T::load(left.value_slice(i, lanes));
        let simd_right = T::load(right.value_slice(i, lanes));
        let simd_result = T::bin_op(simd_left, simd_right, &op);

        let result_slice: &mut [T::Native] = unsafe {
            from_raw_parts_mut(
                (result.data_mut().as_mut_ptr() as *mut T::Native).add(i),
                lanes,
            )
        };
        T::write(simd_result, result_slice);
    }

    let data = ArrayData::new(
        T::DATA_TYPE,
        left.len(),
        None,
        null_bit_buffer,
        0,
        vec![result.freeze()],
        vec![],
    );
    Ok(PrimitiveArray::<T>::from(Arc::new(data)))
}

/// SIMD vectorized version of `divide`, the divide kernel needs it's own implementation as there
/// is a need to handle situations where a divide by `0` occurs.  This is complicated by `NULL`
/// slots and padding.
#[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
fn simd_divide<T>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Native: One + Zero,
{
    if left.len() != right.len() {
        return Err(ArrowError::ComputeError(
            "Cannot perform math operation on arrays of different length".to_string(),
        ));
    }

    // Create the combined `Bitmap`
    let null_bit_buffer =
        combine_option_bitmap(left.data_ref(), right.data_ref(), left.len())?;
    let bitmap = null_bit_buffer.clone().map(Bitmap::from);

    let lanes = T::lanes();
    let buffer_size = left.len() * mem::size_of::<T::Native>();
    let mut result = MutableBuffer::new(buffer_size).with_bitset(buffer_size, false);

    for i in (0..left.len()).step_by(lanes) {
        let right_no_invalid_zeros =
            unsafe { simd_load_set_invalid(right, &bitmap, i, lanes, T::Native::one()) };
        let is_zero = T::eq(T::init(T::Native::zero()), right_no_invalid_zeros);
        if T::mask_any(is_zero) {
            return Err(ArrowError::DivideByZero);
        }
        let simd_left = T::load(left.value_slice(i, lanes));
        let simd_result = T::bin_op(simd_left, right_no_invalid_zeros, |a, b| a / b);

        let result_slice: &mut [T::Native] = unsafe {
            from_raw_parts_mut(
                (result.data_mut().as_mut_ptr() as *mut T::Native).add(i),
                lanes,
            )
        };
        T::write(simd_result, result_slice);
    }

    let data = ArrayData::new(
        T::DATA_TYPE,
        left.len(),
        None,
        null_bit_buffer,
        0,
        vec![result.freeze()],
        vec![],
    );
    Ok(PrimitiveArray::<T>::from(Arc::new(data)))
}

/// Perform `left + right` operation on two arrays. If either left or right value is null
/// then the result is also null.
pub fn add<T>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Native: Add<Output = T::Native>
        + Sub<Output = T::Native>
        + Mul<Output = T::Native>
        + Div<Output = T::Native>
        + Zero,
{
    #[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
    return simd_math_op(&left, &right, |a, b| a + b);

    #[allow(unreachable_code)]
    math_op(left, right, |a, b| a + b)
}

/// Perform `left - right` operation on two arrays. If either left or right value is null
/// then the result is also null.
pub fn subtract<T>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Native: Add<Output = T::Native>
        + Sub<Output = T::Native>
        + Mul<Output = T::Native>
        + Div<Output = T::Native>
        + Zero,
{
    #[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
    return simd_math_op(&left, &right, |a, b| a - b);

    #[allow(unreachable_code)]
    math_op(left, right, |a, b| a - b)
}

/// Perform `left * right` operation on two arrays. If either left or right value is null
/// then the result is also null.
pub fn multiply<T>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Native: Add<Output = T::Native>
        + Sub<Output = T::Native>
        + Mul<Output = T::Native>
        + Div<Output = T::Native>
        + Zero,
{
    #[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
    return simd_math_op(&left, &right, |a, b| a * b);

    #[allow(unreachable_code)]
    math_op(left, right, |a, b| a * b)
}

/// Perform `left / right` operation on two arrays. If either left or right value is null
/// then the result is also null. If any right hand value is zero then the result of this
/// operation will be `Err(ArrowError::DivideByZero)`.
pub fn divide<T>(
    left: &PrimitiveArray<T>,
    right: &PrimitiveArray<T>,
) -> Result<PrimitiveArray<T>>
where
    T: datatypes::ArrowNumericType,
    T::Native: Add<Output = T::Native>
        + Sub<Output = T::Native>
        + Mul<Output = T::Native>
        + Div<Output = T::Native>
        + Zero
        + One,
{
    #[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "simd"))]
    return simd_divide(&left, &right);

    #[allow(unreachable_code)]
    math_divide(&left, &right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::Int32Array;

    #[test]
    fn test_primitive_array_add() {
        let a = Int32Array::from(vec![5, 6, 7, 8, 9]);
        let b = Int32Array::from(vec![6, 7, 8, 9, 8]);
        let c = add(&a, &b).unwrap();
        assert_eq!(11, c.value(0));
        assert_eq!(13, c.value(1));
        assert_eq!(15, c.value(2));
        assert_eq!(17, c.value(3));
        assert_eq!(17, c.value(4));
    }

    #[test]
    fn test_primitive_array_add_sliced() {
        let a = Int32Array::from(vec![0, 0, 0, 5, 6, 7, 8, 9, 0]);
        let b = Int32Array::from(vec![0, 0, 0, 6, 7, 8, 9, 8, 0]);
        let a = a.slice(3, 5);
        let b = b.slice(3, 5);
        let a = a.as_any().downcast_ref::<Int32Array>().unwrap();
        let b = b.as_any().downcast_ref::<Int32Array>().unwrap();

        assert_eq!(5, a.value(0));
        assert_eq!(6, b.value(0));

        let c = add(&a, &b).unwrap();
        assert_eq!(5, c.len());
        assert_eq!(11, c.value(0));
        assert_eq!(13, c.value(1));
        assert_eq!(15, c.value(2));
        assert_eq!(17, c.value(3));
        assert_eq!(17, c.value(4));
    }

    #[test]
    fn test_primitive_array_add_mismatched_length() {
        let a = Int32Array::from(vec![5, 6, 7, 8, 9]);
        let b = Int32Array::from(vec![6, 7, 8]);
        let e = add(&a, &b)
            .err()
            .expect("should have failed due to different lengths");
        assert_eq!(
            "ComputeError(\"Cannot perform math operation on arrays of different length\")",
            format!("{:?}", e)
        );
    }

    #[test]
    fn test_primitive_array_subtract() {
        let a = Int32Array::from(vec![1, 2, 3, 4, 5]);
        let b = Int32Array::from(vec![5, 4, 3, 2, 1]);
        let c = subtract(&a, &b).unwrap();
        assert_eq!(-4, c.value(0));
        assert_eq!(-2, c.value(1));
        assert_eq!(0, c.value(2));
        assert_eq!(2, c.value(3));
        assert_eq!(4, c.value(4));
    }

    #[test]
    fn test_primitive_array_multiply() {
        let a = Int32Array::from(vec![5, 6, 7, 8, 9]);
        let b = Int32Array::from(vec![6, 7, 8, 9, 8]);
        let c = multiply(&a, &b).unwrap();
        assert_eq!(30, c.value(0));
        assert_eq!(42, c.value(1));
        assert_eq!(56, c.value(2));
        assert_eq!(72, c.value(3));
        assert_eq!(72, c.value(4));
    }

    #[test]
    fn test_primitive_array_divide() {
        let a = Int32Array::from(vec![15, 15, 8, 1, 9]);
        let b = Int32Array::from(vec![5, 6, 8, 9, 1]);
        let c = divide(&a, &b).unwrap();
        assert_eq!(3, c.value(0));
        assert_eq!(2, c.value(1));
        assert_eq!(1, c.value(2));
        assert_eq!(0, c.value(3));
        assert_eq!(9, c.value(4));
    }

    #[test]
    fn test_primitive_array_divide_sliced() {
        let a = Int32Array::from(vec![0, 0, 0, 15, 15, 8, 1, 9, 0]);
        let b = Int32Array::from(vec![0, 0, 0, 5, 6, 8, 9, 1, 0]);
        let a = a.slice(3, 5);
        let b = b.slice(3, 5);
        let a = a.as_any().downcast_ref::<Int32Array>().unwrap();
        let b = b.as_any().downcast_ref::<Int32Array>().unwrap();

        let c = divide(&a, &b).unwrap();
        assert_eq!(5, c.len());
        assert_eq!(3, c.value(0));
        assert_eq!(2, c.value(1));
        assert_eq!(1, c.value(2));
        assert_eq!(0, c.value(3));
        assert_eq!(9, c.value(4));
    }

    #[test]
    fn test_primitive_array_divide_with_nulls() {
        let a = Int32Array::from(vec![Some(15), None, Some(8), Some(1), Some(9), None]);
        let b = Int32Array::from(vec![Some(5), Some(6), Some(8), Some(9), None, None]);
        let c = divide(&a, &b).unwrap();
        assert_eq!(3, c.value(0));
        assert_eq!(true, c.is_null(1));
        assert_eq!(1, c.value(2));
        assert_eq!(0, c.value(3));
        assert_eq!(true, c.is_null(4));
        assert_eq!(true, c.is_null(5));
    }

    #[test]
    fn test_primitive_array_divide_with_nulls_sliced() {
        let a = Int32Array::from(vec![
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(15),
            None,
            Some(8),
            Some(1),
            Some(9),
            None,
            None,
        ]);
        let b = Int32Array::from(vec![
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(5),
            Some(6),
            Some(8),
            Some(9),
            None,
            None,
            None,
        ]);

        let a = a.slice(8, 6);
        let a = a.as_any().downcast_ref::<Int32Array>().unwrap();

        let b = b.slice(8, 6);
        let b = b.as_any().downcast_ref::<Int32Array>().unwrap();

        let c = divide(&a, &b).unwrap();
        assert_eq!(6, c.len());
        assert_eq!(3, c.value(0));
        assert_eq!(true, c.is_null(1));
        assert_eq!(1, c.value(2));
        assert_eq!(0, c.value(3));
        assert_eq!(true, c.is_null(4));
        assert_eq!(true, c.is_null(5));
    }

    #[test]
    #[should_panic(expected = "DivideByZero")]
    fn test_primitive_array_divide_by_zero() {
        let a = Int32Array::from(vec![15]);
        let b = Int32Array::from(vec![0]);
        divide(&a, &b).unwrap();
    }

    #[test]
    fn test_primitive_array_divide_f64() {
        let a = Float64Array::from(vec![15.0, 15.0, 8.0]);
        let b = Float64Array::from(vec![5.0, 6.0, 8.0]);
        let c = divide(&a, &b).unwrap();
        assert!(3.0 - c.value(0) < f64::EPSILON);
        assert!(2.5 - c.value(1) < f64::EPSILON);
        assert!(1.0 - c.value(2) < f64::EPSILON);
    }

    #[test]
    fn test_primitive_array_add_with_nulls() {
        let a = Int32Array::from(vec![Some(5), None, Some(7), None]);
        let b = Int32Array::from(vec![None, None, Some(6), Some(7)]);
        let c = add(&a, &b).unwrap();
        assert_eq!(true, c.is_null(0));
        assert_eq!(true, c.is_null(1));
        assert_eq!(false, c.is_null(2));
        assert_eq!(true, c.is_null(3));
        assert_eq!(13, c.value(2));
    }
}
