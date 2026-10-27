//! Tab stops.

/// Horizontal tab stops, one flag per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStops {
    stops: Vec<bool>,
}

impl TabStops {
    /// Default spacing between tab stops.
    pub const DEFAULT_INTERVAL: u16 = 8;

    /// Tab stops every [`Self::DEFAULT_INTERVAL`] columns over `cols` columns.
    pub fn new(cols: u16) -> Self {
        let stops = (0..cols)
            .map(|col| col % Self::DEFAULT_INTERVAL == 0)
            .collect();
        Self { stops }
    }

    /// Number of columns covered.
    pub fn cols(&self) -> u16 {
        u16::try_from(self.stops.len()).unwrap_or(u16::MAX)
    }

    /// True when `col` is a tab stop.
    pub fn is_set(&self, col: u16) -> bool {
        self.stops.get(usize::from(col)).copied().unwrap_or(false)
    }

    /// Sets a tab stop at `col`. Out-of-range columns are ignored.
    pub fn set(&mut self, col: u16) {
        if let Some(stop) = self.stops.get_mut(usize::from(col)) {
            *stop = true;
        }
    }

    /// Clears the tab stop at `col`, if any.
    pub fn clear(&mut self, col: u16) {
        if let Some(stop) = self.stops.get_mut(usize::from(col)) {
            *stop = false;
        }
    }

    /// Clears every tab stop.
    pub fn clear_all(&mut self) {
        self.stops.fill(false);
    }

    /// Restores the default stops.
    pub fn reset(&mut self) {
        *self = Self::new(self.cols());
    }

    /// The column of the `n`th tab stop strictly after `col`, or `None`
    /// when fewer than `n` stops remain. `n == 0` yields `None`.
    pub fn next(&self, col: u16, n: u16) -> Option<u16> {
        if n == 0 {
            return None;
        }
        let start = usize::from(col).saturating_add(1);
        self.stops
            .iter()
            .enumerate()
            .skip(start)
            .filter(|&(_, &set)| set)
            .nth(usize::from(n) - 1)
            .and_then(|(i, _)| u16::try_from(i).ok())
    }

    /// The column of the `n`th tab stop strictly before `col`, or `None`
    /// when fewer than `n` stops precede it. `n == 0` yields `None`.
    pub fn previous(&self, col: u16, n: u16) -> Option<u16> {
        if n == 0 {
            return None;
        }
        let end = usize::from(col).min(self.stops.len());
        self.stops[..end]
            .iter()
            .enumerate()
            .rev()
            .filter(|&(_, &set)| set)
            .nth(usize::from(n) - 1)
            .and_then(|(i, _)| u16::try_from(i).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_every_eight() {
        let tabs = TabStops::new(20);
        let set: Vec<u16> = (0..20).filter(|&c| tabs.is_set(c)).collect();
        assert_eq!(set, vec![0, 8, 16]);
        assert_eq!(tabs.cols(), 20);
    }

    #[test]
    fn next_and_previous() {
        let tabs = TabStops::new(20);
        assert_eq!(tabs.next(0, 1), Some(8));
        assert_eq!(tabs.next(0, 2), Some(16));
        assert_eq!(tabs.next(0, 3), None);
        assert_eq!(tabs.next(8, 1), Some(16));
        assert_eq!(tabs.next(16, 1), None);
        assert_eq!(tabs.next(19, 1), None);
        assert_eq!(tabs.next(0, 0), None);
        assert_eq!(tabs.previous(19, 1), Some(16));
        assert_eq!(tabs.previous(16, 1), Some(8));
        assert_eq!(tabs.previous(16, 2), Some(0));
        assert_eq!(tabs.previous(16, 3), None);
        assert_eq!(tabs.previous(0, 1), None);
        assert_eq!(tabs.previous(5, 0), None);
    }

    #[test]
    fn set_clear_reset() {
        let mut tabs = TabStops::new(10);
        tabs.set(3);
        assert!(tabs.is_set(3));
        assert_eq!(tabs.next(0, 1), Some(3));
        tabs.clear(3);
        assert_eq!(tabs.next(0, 1), Some(8));
        tabs.clear_all();
        assert_eq!(tabs.next(0, 1), None);
        assert!(!tabs.is_set(0));
        tabs.reset();
        assert_eq!(tabs, TabStops::new(10));
    }

    #[test]
    fn out_of_range_is_ignored() {
        let mut tabs = TabStops::new(4);
        tabs.set(100);
        tabs.clear(100);
        assert!(!tabs.is_set(100));
        assert_eq!(tabs.next(100, 1), None);
        assert_eq!(tabs.previous(100, 1), Some(0));
    }
}
