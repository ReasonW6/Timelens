use std::{
    fmt,
    ops::{Index, IndexMut, Range},
    sync::Arc,
};

/// Immutable history shared between views, with independent edits for live rows.
#[derive(Clone)]
pub struct TimelineSeries<T> {
    base: Arc<Vec<T>>,
    updates: Vec<(usize, T)>,
    order: Option<Arc<Vec<usize>>>,
}

impl<T> Default for TimelineSeries<T> {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl<T> From<Vec<T>> for TimelineSeries<T> {
    fn from(base: Vec<T>) -> Self {
        Self {
            base: Arc::new(base),
            updates: Vec::new(),
            order: None,
        }
    }
}

impl<T> FromIterator<T> for TimelineSeries<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl<T> TimelineSeries<T> {
    pub fn len(&self) -> usize {
        self.base.len()
    }

    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        (index < self.len()).then(|| &self[index])
    }

    pub fn iter(&self) -> TimelineSeriesIter<'_, T> {
        self.into_iter()
    }

    fn physical_index(&self, index: usize) -> usize {
        self.order.as_ref().map_or(index, |order| order[index])
    }

    fn value(&self, index: usize) -> &T {
        self.updates
            .iter()
            .find(|(row, _)| *row == index)
            .map_or(&self.base[index], |(_, value)| value)
    }
}

impl<T: Clone> TimelineSeries<T> {
    pub fn push(&mut self, value: T) {
        if !self.updates.is_empty() || self.order.is_some() {
            *self = self.iter().cloned().collect::<Vec<_>>().into();
        }
        Arc::make_mut(&mut self.base).push(value);
    }

    pub fn sort_unstable_by_key<K: Ord>(&mut self, mut key: impl FnMut(&T) -> K) {
        if self
            .iter()
            .zip(self.iter().skip(1))
            .all(|(a, b)| key(a) <= key(b))
        {
            return;
        }
        let mut order = (0..self.len())
            .map(|index| self.physical_index(index))
            .collect::<Vec<_>>();
        order.sort_unstable_by_key(|index| key(self.value(*index)));
        self.order = Some(Arc::new(order));
    }
}

impl<T> Index<usize> for TimelineSeries<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        self.value(self.physical_index(index))
    }
}

impl<T: Clone> IndexMut<usize> for TimelineSeries<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        let index = self.physical_index(index);
        let updated = self.updates.iter().position(|(row, _)| *row == index);
        let position = updated.unwrap_or_else(|| {
            self.updates.push((index, self.base[index].clone()));
            self.updates.len() - 1
        });
        &mut self.updates[position].1
    }
}

pub struct TimelineSeriesIter<'a, T> {
    series: &'a TimelineSeries<T>,
    indices: Range<usize>,
}

impl<'a, T> IntoIterator for &'a TimelineSeries<T> {
    type Item = &'a T;
    type IntoIter = TimelineSeriesIter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        TimelineSeriesIter {
            series: self,
            indices: 0..self.len(),
        }
    }
}

impl<'a, T> Iterator for TimelineSeriesIter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        self.indices.next().map(|index| &self.series[index])
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl<T> DoubleEndedIterator for TimelineSeriesIter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.indices.next_back().map(|index| &self.series[index])
    }
}
impl<T> ExactSizeIterator for TimelineSeriesIter<'_, T> {}
impl<T> std::iter::FusedIterator for TimelineSeriesIter<'_, T> {}

impl<T: PartialEq> PartialEq for TimelineSeries<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}
impl<T: Eq> Eq for TimelineSeries<T> {}
impl<T: fmt::Debug> fmt::Debug for TimelineSeries<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_rows_remain_shared_while_live_updates_are_independent() {
        let original: TimelineSeries<_> = vec![10, 20, 30].into();
        let mut later = original.clone();
        for value in 31..100 {
            later[2] = value;
        }
        assert!(Arc::ptr_eq(&original.base, &later.base));
        assert_eq!(later.updates.len(), 1);
        assert_eq!(original.iter().copied().collect::<Vec<_>>(), [10, 20, 30]);
        assert_eq!(later.iter().copied().collect::<Vec<_>>(), [10, 20, 99]);
    }

    #[test]
    fn reordering_and_append_preserve_values_and_previous_views() {
        let original: TimelineSeries<_> = vec![2, 1, 3].into();
        let mut later = original.clone();
        later[1] = 4;
        later.sort_unstable_by_key(|value| *value);
        assert!(Arc::ptr_eq(&original.base, &later.base));
        assert_eq!(later, vec![2, 3, 4].into());
        later[1] = 7;
        later.push(8);
        assert_eq!(later, vec![2, 7, 4, 8].into());
        assert_eq!(original, vec![2, 1, 3].into());
        assert_eq!(
            later.iter().rev().copied().collect::<Vec<_>>(),
            [8, 4, 7, 2]
        );
    }
}
