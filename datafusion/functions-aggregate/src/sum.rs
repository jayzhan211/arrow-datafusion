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

//! Defines `SUM` and `SUM DISTINCT` aggregate accumulators

use std::any::Any;

use arrow::array::Array;
use arrow::array::ArrowNativeTypeOp;
use arrow::array::{ArrowNumericType, AsArray};
use arrow::datatypes::ArrowNativeType;
use arrow::datatypes::{
    DataType, Decimal128Type, Decimal256Type, Float64Type, Int64Type, UInt64Type,
    DECIMAL128_MAX_PRECISION, DECIMAL256_MAX_PRECISION,
};
use arrow::{array::ArrayRef, datatypes::Field};
use datafusion_common::{exec_err, not_impl_err, Result, ScalarValue};
use datafusion_expr::function::AccumulatorArgs;
use datafusion_expr::function::StateFieldsArgs;
use datafusion_expr::type_coercion::aggregates::NUMERICS;
use datafusion_expr::utils::format_state_name;
use datafusion_expr::{
    Accumulator, AggregateUDFImpl, GroupsAccumulator, ReversedUDAF, Signature, Volatility,
};
use datafusion_physical_expr_common::aggregate::groups_accumulator::prim_op::PrimitiveGroupsAccumulator;

make_udaf_expr_and_func!(
    Sum,
    sum,
    expression,
    "Returns the first value in a group of values.",
    sum_udaf
);

/// Sum only supports a subset of numeric types, instead relying on type coercion
///
/// This macro is similar to [downcast_primitive](arrow::array::downcast_primitive)
///
/// `args` is [AccumulatorArgs]
/// `helper` is a macro accepting (ArrowPrimitiveType, DataType)
macro_rules! downcast_sum {
    ($args:ident, $helper:ident) => {
        match $args.data_type {
            DataType::UInt64 => $helper!(UInt64Type, $args.data_type),
            DataType::Int64 => $helper!(Int64Type, $args.data_type),
            DataType::Float64 => $helper!(Float64Type, $args.data_type),
            DataType::Decimal128(_, _) => $helper!(Decimal128Type, $args.data_type),
            DataType::Decimal256(_, _) => $helper!(Decimal256Type, $args.data_type),
            _ => {
                not_impl_err!("Sum not supported for {}: {}", $args.name, $args.data_type)
            }
        }
    };
}

#[derive(Debug)]
pub struct Sum {
    signature: Signature,
    aliases: Vec<String>,
}

impl Sum {
    pub fn new() -> Self {
        Self {
            signature: Signature::uniform(1, NUMERICS.to_vec(), Volatility::Immutable),
            aliases: vec!["sum".to_string()],
        }
    }
}

impl Default for Sum {
    fn default() -> Self {
        Self::new()
    }
}

impl AggregateUDFImpl for Sum {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "SUM"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        // Refer to https://www.postgresql.org/docs/8.2/functions-aggregate.html doc
        // smallint, int, bigint, real, double precision, decimal, or interval.

        fn coerced_types(data_type: &DataType) -> Result<DataType> {
            match data_type {
                DataType::Dictionary(_, v) => coerced_types(v),
                // in the spark, the result type is DECIMAL(min(38,precision+10), s)
                // ref: https://github.com/apache/spark/blob/fcf636d9eb8d645c24be3db2d599aba2d7e2955a/sql/catalyst/src/main/scala/org/apache/spark/sql/catalyst/expressions/aggregate/Sum.scala#L66
                DataType::Decimal128(precision, scale) => {
                    let new_precision = DECIMAL128_MAX_PRECISION.min(*precision + 10);
                    Ok(DataType::Decimal128(new_precision, *scale))
                }
                DataType::Decimal256(precision, scale) => {
                    let new_precision = DECIMAL256_MAX_PRECISION.min(*precision + 10);
                    Ok(DataType::Decimal256(new_precision, *scale))
                }
                dt if dt.is_signed_integer() => Ok(DataType::Int64),
                dt if dt.is_unsigned_integer() => Ok(DataType::UInt64),
                dt if dt.is_floating() => Ok(DataType::Float64),
                _ => exec_err!("Sum not supported for {}", data_type),
            }
        }

        coerced_types(&arg_types[0])
    }

    fn accumulator(&self, args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        macro_rules! helper {
            ($t:ty, $dt:expr) => {
                Ok(Box::new(SumAccumulator::<$t>::new($dt.clone())))
            };
        }
        downcast_sum!(args, helper)
    }

    fn state_fields(&self, args: StateFieldsArgs) -> Result<Vec<Field>> {
        Ok(vec![Field::new(
            format_state_name(args.name, "sum"),
            args.return_type.clone(),
            true,
        )])
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }

    fn groups_accumulator_supported(&self, _args: AccumulatorArgs) -> bool {
        true
    }

    fn create_groups_accumulator(
        &self,
        args: AccumulatorArgs,
    ) -> Result<Box<dyn GroupsAccumulator>> {
        macro_rules! helper {
            ($t:ty, $dt:expr) => {
                Ok(Box::new(PrimitiveGroupsAccumulator::<$t, _>::new(
                    &$dt,
                    |x, y| *x = x.add_wrapping(y),
                )))
            };
        }
        downcast_sum!(args, helper)
    }

    fn create_sliding_accumulator(
        &self,
        args: AccumulatorArgs,
    ) -> Result<Box<dyn Accumulator>> {
        macro_rules! helper {
            ($t:ty, $dt:expr) => {
                Ok(Box::new(SlidingSumAccumulator::<$t>::new($dt.clone())))
            };
        }
        downcast_sum!(args, helper)
    }

    fn reverse_expr(&self) -> ReversedUDAF {
        ReversedUDAF::Identical
    }
}

/// This accumulator computes SUM incrementally
struct SumAccumulator<T: ArrowNumericType> {
    sum: Option<T::Native>,
    data_type: DataType,
}

impl<T: ArrowNumericType> std::fmt::Debug for SumAccumulator<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SumAccumulator({})", self.data_type)
    }
}

impl<T: ArrowNumericType> SumAccumulator<T> {
    fn new(data_type: DataType) -> Self {
        Self {
            sum: None,
            data_type,
        }
    }
}

impl<T: ArrowNumericType> Accumulator for SumAccumulator<T> {
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![self.evaluate()?])
    }

    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<T>();
        if let Some(x) = arrow::compute::sum(values) {
            let v = self.sum.get_or_insert(T::Native::usize_as(0));
            *v = v.add_wrapping(x);
        }
        Ok(())
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        self.update_batch(states)
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        ScalarValue::new_primitive::<T>(self.sum, &self.data_type)
    }

    fn size(&self) -> usize {
        std::mem::size_of_val(self)
    }
}

/// This accumulator incrementally computes sums over a sliding window
///
/// This is separate from [`SumAccumulator`] as requires additional state
struct SlidingSumAccumulator<T: ArrowNumericType> {
    sum: T::Native,
    count: u64,
    data_type: DataType,
}

impl<T: ArrowNumericType> std::fmt::Debug for SlidingSumAccumulator<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SlidingSumAccumulator({})", self.data_type)
    }
}

impl<T: ArrowNumericType> SlidingSumAccumulator<T> {
    fn new(data_type: DataType) -> Self {
        Self {
            sum: T::Native::usize_as(0),
            count: 0,
            data_type,
        }
    }
}

impl<T: ArrowNumericType> Accumulator for SlidingSumAccumulator<T> {
    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![self.evaluate()?, self.count.into()])
    }

    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<T>();
        self.count += (values.len() - values.null_count()) as u64;
        if let Some(x) = arrow::compute::sum(values) {
            self.sum = self.sum.add_wrapping(x)
        }
        Ok(())
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        let values = states[0].as_primitive::<T>();
        if let Some(x) = arrow::compute::sum(values) {
            self.sum = self.sum.add_wrapping(x)
        }
        if let Some(x) = arrow::compute::sum(states[1].as_primitive::<UInt64Type>()) {
            self.count += x;
        }
        Ok(())
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        let v = (self.count != 0).then_some(self.sum);
        ScalarValue::new_primitive::<T>(v, &self.data_type)
    }

    fn size(&self) -> usize {
        std::mem::size_of_val(self)
    }

    fn retract_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = values[0].as_primitive::<T>();
        if let Some(x) = arrow::compute::sum(values) {
            self.sum = self.sum.sub_wrapping(x)
        }
        self.count -= (values.len() - values.null_count()) as u64;
        Ok(())
    }

    fn supports_retract_batch(&self) -> bool {
        true
    }
}
