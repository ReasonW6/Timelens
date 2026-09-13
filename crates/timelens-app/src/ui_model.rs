//! Keep native row instances alive while their displayed values refresh.
use slint::{Model, ModelRc, VecModel};

pub fn sync<T: Clone + PartialEq + 'static>(
    current: ModelRc<T>,
    rows: Vec<T>,
    same_identity: impl Fn(&T, &T) -> bool,
) -> ModelRc<T> {
    let reusable = current.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, row)| {
            current
                .row_data(index)
                .is_some_and(|old| same_identity(&old, row))
        });
    if reusable && let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
        current
    } else {
        ModelRc::new(VecModel::from(rows))
    }
}
