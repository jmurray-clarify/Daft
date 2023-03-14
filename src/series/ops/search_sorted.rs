use crate::{
    datatypes::UInt64Array,
    error::DaftResult,
    series::{ops::match_types_on_series, Series},
    with_match_comparable_daft_types,
};

impl Series {
    pub fn search_sorted(&self, keys: &Self, descending: bool) -> DaftResult<UInt64Array> {
        let (lhs, rhs) = match_types_on_series(self, keys)?;
        with_match_comparable_daft_types!(lhs.data_type(), |$T| {
            let lhs = lhs.downcast::<$T>().unwrap();
            let rhs = rhs.downcast::<$T>().unwrap();
            lhs.search_sorted(rhs, descending)
        })
    }
}
