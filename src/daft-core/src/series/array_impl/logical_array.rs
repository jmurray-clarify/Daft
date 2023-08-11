use crate::datatypes::logical::{
    DateArray, Decimal128Array, DurationArray, EmbeddingArray, FixedShapeImageArray,
    FixedShapeTensorArray, ImageArray, TensorArray, TimestampArray,
};
use crate::datatypes::BooleanArray;

use super::{ArrayWrapper, IntoSeries, Series};
use crate::array::ops::GroupIndices;
use crate::series::array_impl::binary_ops::SeriesBinaryOps;
use crate::series::DaftResult;
use crate::series::SeriesLike;
use crate::with_match_daft_logical_primitive_types;
use crate::with_match_integer_daft_types;
use std::sync::Arc;

macro_rules! impl_series_like_for_logical_array {
    ($da:ident) => {
        impl IntoSeries for $da {
            fn into_series(self) -> Series {
                Series {
                    inner: Arc::new(ArrayWrapper(self)),
                }
            }
        }

        impl SeriesLike for ArrayWrapper<$da> {
            fn into_series(&self) -> Series {
                self.0.clone().into_series()
            }

            fn to_arrow(&self) -> Box<dyn arrow2::array::Array> {
                let daft_type = self.0.logical_type();
                let arrow_logical_type = daft_type.to_arrow().unwrap();
                let physical_arrow_array = self.0.physical.0.data();
                use crate::datatypes::DataType::*;
                match daft_type {
                    // For wrapped primitive types, switch the datatype label on the arrow2 Array.
                    Decimal128(..) | Date | Timestamp(..) | Duration(..) => {
                        with_match_daft_logical_primitive_types!(daft_type, |$P| {
                            use arrow2::array::Array;
                            physical_arrow_array
                                .as_any()
                                .downcast_ref::<arrow2::array::PrimitiveArray<$P>>()
                                .unwrap()
                                .clone()
                                .to(arrow_logical_type)
                                .to_boxed()
                        })
                    }
                    // Otherwise, use arrow cast to make sure the result arrow2 array is of the correct type.
                    _ => arrow2::compute::cast::cast(
                        physical_arrow_array,
                        &arrow_logical_type,
                        arrow2::compute::cast::CastOptions {
                            wrapped: true,
                            partial: false,
                        },
                    )
                    .unwrap(),
                }
            }

            fn as_arrow(&self) -> Box<dyn arrow2::array::Array> {
                // TODO(jay): Figure out if this is the correct behavior? Apparently to_arrow is FFI.
                self.to_arrow()
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }

            fn broadcast(&self, num: usize) -> DaftResult<Series> {
                use crate::array::ops::broadcast::Broadcastable;
                let data_array = self.0.physical.0.broadcast(num)?;
                Ok($da::new(self.0.field.clone(), data_array).into_series())
            }

            fn cast(&self, datatype: &crate::datatypes::DataType) -> DaftResult<Series> {
                self.0.cast(datatype)
            }

            fn data_type(&self) -> &crate::datatypes::DataType {
                self.0.logical_type()
            }

            fn field(&self) -> &crate::datatypes::Field {
                self.0.field()
            }

            fn filter(&self, mask: &crate::datatypes::BooleanArray) -> DaftResult<Series> {
                // TODO: This seems wrong? We need to wrap it back into the correct logical type
                Ok(self.0.physical.0.filter(mask)?.into_series())
            }

            fn head(&self, num: usize) -> DaftResult<Series> {
                // TODO: This seems wrong? We need to wrap it back into the correct logical type
                Ok(self.0.physical.0.head(num)?.into_series())
            }

            fn if_else(&self, other: &Series, predicate: &Series) -> DaftResult<Series> {
                Ok(self
                    .0
                    .if_else(other.downcast_logical()?, predicate.downcast()?)?
                    .into_series())
            }

            fn is_null(&self) -> DaftResult<Series> {
                use crate::array::ops::DaftIsNull;

                Ok(DaftIsNull::is_null(&self.0.physical.0)?.into_series())
            }

            fn len(&self) -> usize {
                self.0.len()
            }

            fn size_bytes(&self) -> DaftResult<usize> {
                self.0.size_bytes()
            }

            fn name(&self) -> &str {
                self.0.name()
            }

            fn rename(&self, name: &str) -> Series {
                self.0.physical.0.rename(name).into_series()
            }

            fn slice(&self, start: usize, end: usize) -> DaftResult<Series> {
                Ok(self.0.physical.0.slice(start, end)?.into_series())
            }

            fn sort(&self, descending: bool) -> DaftResult<Series> {
                Ok(self.0.sort(descending)?.into_series())
            }

            fn str_value(&self, idx: usize) -> DaftResult<String> {
                self.0.str_value(idx)
            }

            fn html_value(&self, idx: usize) -> String {
                self.0.html_value(idx)
            }

            fn take(&self, idx: &Series) -> DaftResult<Series> {
                with_match_integer_daft_types!(idx.data_type(), |$S| {
                    Ok(self.0.take(idx.downcast::<$S>()?)?.into_series())
                })
            }

            fn min(&self, groups: Option<&GroupIndices>) -> DaftResult<Series> {
                use crate::array::ops::DaftCompareAggable;
                let data_array = match groups {
                    Some(groups) => DaftCompareAggable::grouped_min(&self.0.physical.0, groups)?,
                    None => DaftCompareAggable::min(&self.0.physical.0)?,
                };
                Ok($da::new(self.0.field.clone(), data_array).into_series())
            }
            fn max(&self, groups: Option<&GroupIndices>) -> DaftResult<Series> {
                use crate::array::ops::DaftCompareAggable;
                let data_array = match groups {
                    Some(groups) => DaftCompareAggable::grouped_max(&self.0.physical.0, groups)?,
                    None => DaftCompareAggable::max(&self.0.physical.0)?,
                };
                Ok($da::new(self.0.field.clone(), data_array).into_series())
            }
            fn agg_list(&self, groups: Option<&GroupIndices>) -> DaftResult<Series> {
                use crate::array::ops::DaftListAggable;
                use crate::datatypes::ListArray;
                let data_array = match groups {
                    Some(groups) => self.0.physical.0.grouped_list(groups)?,
                    None => self.0.physical.0.list()?,
                };
                let new_field = self.field().to_list_field()?;
                Ok(ListArray::new(Arc::new(new_field), data_array.data)?.into_series())
            }

            fn add(&self, rhs: &Series) -> DaftResult<Series> {
                SeriesBinaryOps::add(self, rhs)
            }

            fn sub(&self, rhs: &Series) -> DaftResult<Series> {
                SeriesBinaryOps::sub(self, rhs)
            }

            fn mul(&self, rhs: &Series) -> DaftResult<Series> {
                SeriesBinaryOps::mul(self, rhs)
            }

            fn div(&self, rhs: &Series) -> DaftResult<Series> {
                SeriesBinaryOps::div(self, rhs)
            }

            fn rem(&self, rhs: &Series) -> DaftResult<Series> {
                SeriesBinaryOps::rem(self, rhs)
            }
            fn and(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::and(self, rhs)
            }
            fn or(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::or(self, rhs)
            }
            fn xor(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::xor(self, rhs)
            }
            fn equal(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::equal(self, rhs)
            }
            fn not_equal(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::not_equal(self, rhs)
            }
            fn lt(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::lt(self, rhs)
            }
            fn lte(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::lte(self, rhs)
            }
            fn gt(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::gt(self, rhs)
            }
            fn gte(&self, rhs: &Series) -> DaftResult<BooleanArray> {
                SeriesBinaryOps::gte(self, rhs)
            }
        }
    };
}

impl_series_like_for_logical_array!(Decimal128Array);
impl_series_like_for_logical_array!(DateArray);
impl_series_like_for_logical_array!(DurationArray);
impl_series_like_for_logical_array!(EmbeddingArray);
impl_series_like_for_logical_array!(ImageArray);
impl_series_like_for_logical_array!(FixedShapeImageArray);
impl_series_like_for_logical_array!(TimestampArray);
impl_series_like_for_logical_array!(TensorArray);
impl_series_like_for_logical_array!(FixedShapeTensorArray);
