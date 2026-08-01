//! Window layout tree and terminal rectangle allocation.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WindowId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SplitAxis {
    Rows,
    Columns,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Direction {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Rect {
    pub(crate) row: usize,
    pub(crate) column: usize,
    pub(crate) rows: usize,
    pub(crate) columns: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Layout {
    Window(WindowId),
    Split {
        axis: SplitAxis,
        size: SplitSize,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SplitSize {
    Equal,
    First(usize),
    Second(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResizeAmount {
    Increase(usize),
    Decrease(usize),
    Exact(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeResult {
    Missing,
    Found,
    Resized,
}

impl Layout {
    pub(crate) fn split(
        &mut self,
        target: WindowId,
        new_window: WindowId,
        axis: SplitAxis,
    ) -> bool {
        match self {
            Self::Window(window) if *window == target => {
                *self = Self::Split {
                    axis,
                    size: SplitSize::Equal,
                    first: Box::new(Self::Window(target)),
                    second: Box::new(Self::Window(new_window)),
                };
                true
            }
            Self::Window(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new_window, axis) || second.split(target, new_window, axis)
            }
        }
    }

    pub(crate) fn remove(self, target: WindowId) -> Option<Self> {
        match self {
            Self::Window(window) => (window != target).then_some(Self::Window(window)),
            Self::Split {
                axis,
                size,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    size,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(layout), None) | (None, Some(layout)) => Some(layout),
                (None, None) => None,
            },
        }
    }

    pub(crate) fn windows(&self) -> Vec<WindowId> {
        let mut windows = Vec::new();
        self.collect_windows(&mut windows);
        windows
    }

    pub(crate) fn rectangles(&self, area: Rect) -> Vec<(WindowId, Rect)> {
        let mut rectangles = Vec::new();
        self.collect_rectangles(area, &mut rectangles);
        rectangles
    }

    pub(crate) fn resize(
        &mut self,
        target: WindowId,
        axis: SplitAxis,
        amount: ResizeAmount,
        area: Rect,
    ) -> bool {
        self.resize_inner(target, axis, amount, area) == ResizeResult::Resized
    }

    pub(crate) fn equalize(&mut self) {
        if let Self::Split {
            size,
            first,
            second,
            ..
        } = self
        {
            *size = SplitSize::Equal;
            first.equalize();
            second.equalize();
        }
    }

    pub(crate) fn map_windows(&self, map: &mut impl FnMut(WindowId) -> WindowId) -> Self {
        match self {
            Self::Window(window) => Self::Window(map(*window)),
            Self::Split {
                axis,
                size,
                first,
                second,
            } => Self::Split {
                axis: *axis,
                size: *size,
                first: Box::new(first.map_windows(map)),
                second: Box::new(second.map_windows(map)),
            },
        }
    }

    fn collect_windows(&self, windows: &mut Vec<WindowId>) {
        match self {
            Self::Window(window) => windows.push(*window),
            Self::Split { first, second, .. } => {
                first.collect_windows(windows);
                second.collect_windows(windows);
            }
        }
    }

    fn collect_rectangles(&self, area: Rect, rectangles: &mut Vec<(WindowId, Rect)>) {
        match self {
            Self::Window(window) => rectangles.push((*window, area)),
            Self::Split {
                axis,
                size,
                first,
                second,
            } => {
                let (first_area, second_area) = split_rect(
                    area,
                    *axis,
                    *size,
                    first.minimum_length(*axis),
                    second.minimum_length(*axis),
                );
                first.collect_rectangles(first_area, rectangles);
                second.collect_rectangles(second_area, rectangles);
            }
        }
    }

    fn resize_inner(
        &mut self,
        target: WindowId,
        requested_axis: SplitAxis,
        amount: ResizeAmount,
        area: Rect,
    ) -> ResizeResult {
        let Self::Split {
            axis,
            size,
            first,
            second,
        } = self
        else {
            return match self {
                Self::Window(window) if *window == target => ResizeResult::Found,
                Self::Window(_) => ResizeResult::Missing,
                Self::Split { .. } => unreachable!(),
            };
        };

        let split_axis = *axis;
        let (first_area, second_area) = split_rect(
            area,
            split_axis,
            *size,
            first.minimum_length(split_axis),
            second.minimum_length(split_axis),
        );
        let (side, current, result) = if first.contains(target) {
            (
                SplitSide::First,
                area_length(first_area, requested_axis),
                first.resize_inner(target, requested_axis, amount, first_area),
            )
        } else if second.contains(target) {
            (
                SplitSide::Second,
                area_length(second_area, requested_axis),
                second.resize_inner(target, requested_axis, amount, second_area),
            )
        } else {
            return ResizeResult::Missing;
        };

        if result == ResizeResult::Resized || split_axis != requested_axis {
            return result;
        }

        let requested = resize_length(current, amount);
        *size = match side {
            SplitSide::First => SplitSize::First(requested),
            SplitSide::Second => SplitSize::Second(requested),
        };
        ResizeResult::Resized
    }

    fn contains(&self, target: WindowId) -> bool {
        match self {
            Self::Window(window) => *window == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn minimum_length(&self, axis: SplitAxis) -> usize {
        match self {
            Self::Window(_) => match axis {
                SplitAxis::Rows => 2,
                SplitAxis::Columns => 4,
            },
            Self::Split {
                axis: split_axis,
                first,
                second,
                ..
            } if *split_axis == axis => first
                .minimum_length(axis)
                .saturating_add(second.minimum_length(axis))
                .saturating_add(usize::from(axis == SplitAxis::Columns)),
            Self::Split { first, second, .. } => {
                first.minimum_length(axis).max(second.minimum_length(axis))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SplitSide {
    First,
    Second,
}

pub(crate) fn window_in_direction(
    rectangles: &[(WindowId, Rect)],
    active: WindowId,
    direction: Direction,
) -> Option<WindowId> {
    let (_, current) = rectangles.iter().find(|(window, _)| *window == active)?;
    let current_row = current.row.saturating_mul(2).saturating_add(current.rows);
    let current_column = current
        .column
        .saturating_mul(2)
        .saturating_add(current.columns);

    rectangles
        .iter()
        .filter(|(window, _)| *window != active)
        .filter_map(|(window, candidate)| {
            let row = candidate
                .row
                .saturating_mul(2)
                .saturating_add(candidate.rows);
            let column = candidate
                .column
                .saturating_mul(2)
                .saturating_add(candidate.columns);
            let rows_overlap = ranges_overlap(
                current.row,
                current.row + current.rows,
                candidate.row,
                candidate.row + candidate.rows,
            );
            let columns_overlap = ranges_overlap(
                current.column,
                current.column + current.columns,
                candidate.column,
                candidate.column + candidate.columns,
            );
            let score = match direction {
                Direction::Left if column < current_column => (
                    !rows_overlap,
                    current_column - column,
                    current_row.abs_diff(row),
                ),
                Direction::Down if row > current_row => (
                    !columns_overlap,
                    row - current_row,
                    current_column.abs_diff(column),
                ),
                Direction::Up if row < current_row => (
                    !columns_overlap,
                    current_row - row,
                    current_column.abs_diff(column),
                ),
                Direction::Right if column > current_column => (
                    !rows_overlap,
                    column - current_column,
                    current_row.abs_diff(row),
                ),
                _ => return None,
            };
            Some((score, *window))
        })
        .min_by_key(|(score, _)| *score)
        .map(|(_, window)| window)
}

fn ranges_overlap(
    first_start: usize,
    first_end: usize,
    second_start: usize,
    second_end: usize,
) -> bool {
    first_start < second_end && second_start < first_end
}

fn split_rect(
    area: Rect,
    axis: SplitAxis,
    size: SplitSize,
    first_minimum: usize,
    second_minimum: usize,
) -> (Rect, Rect) {
    match axis {
        SplitAxis::Rows => {
            let (first_rows, second_rows) =
                split_lengths(area.rows, 0, first_minimum, second_minimum, size);
            (
                Rect {
                    rows: first_rows,
                    ..area
                },
                Rect {
                    row: area.row + first_rows,
                    rows: second_rows,
                    ..area
                },
            )
        }
        SplitAxis::Columns => {
            let separator = usize::from(area.columns > 0);
            let (first_columns, second_columns) =
                split_lengths(area.columns, separator, first_minimum, second_minimum, size);
            (
                Rect {
                    columns: first_columns,
                    ..area
                },
                Rect {
                    column: area.column + first_columns + separator,
                    columns: second_columns,
                    ..area
                },
            )
        }
    }
}

fn split_lengths(
    total: usize,
    separator: usize,
    first_minimum: usize,
    second_minimum: usize,
    size: SplitSize,
) -> (usize, usize) {
    let usable = total.saturating_sub(separator);
    let (first_minimum, first_maximum, second_minimum, second_maximum) =
        if first_minimum.saturating_add(second_minimum) <= usable {
            (
                first_minimum,
                usable - second_minimum,
                second_minimum,
                usable - first_minimum,
            )
        } else {
            let middle = usable / 2;
            (middle, middle, usable - middle, usable - middle)
        };
    let first = match size {
        SplitSize::Equal => (usable / 2).clamp(first_minimum, first_maximum),
        SplitSize::First(requested) => requested.clamp(first_minimum, first_maximum),
        SplitSize::Second(requested) => {
            usable.saturating_sub(requested.clamp(second_minimum, second_maximum))
        }
    };
    (first, usable - first)
}

fn resize_length(current: usize, amount: ResizeAmount) -> usize {
    match amount {
        ResizeAmount::Increase(amount) => current.saturating_add(amount),
        ResizeAmount::Decrease(amount) => current.saturating_sub(amount),
        ResizeAmount::Exact(amount) => amount,
    }
}

fn area_length(area: Rect, axis: SplitAxis) -> usize {
    match axis {
        SplitAxis::Rows => area.rows,
        SplitAxis::Columns => area.columns,
    }
}

#[cfg(test)]
mod tests {
    use super::{Direction, Layout, Rect, ResizeAmount, SplitAxis, WindowId, window_in_direction};

    #[test]
    fn splits_and_collapses_a_layout_tree() {
        let one = WindowId(1);
        let two = WindowId(2);
        let three = WindowId(3);
        let mut layout = Layout::Window(one);

        assert!(layout.split(one, two, SplitAxis::Columns));
        assert!(layout.split(two, three, SplitAxis::Rows));
        assert_eq!(layout.windows(), vec![one, two, three]);

        let rectangles = layout.rectangles(Rect {
            rows: 10,
            columns: 21,
            ..Rect::default()
        });
        assert_eq!(rectangles[0].1.columns, 10);
        assert_eq!(
            rectangles[1].1,
            Rect {
                row: 0,
                column: 11,
                rows: 5,
                columns: 10,
            }
        );
        assert_eq!(
            window_in_direction(&rectangles, one, Direction::Right),
            Some(two)
        );
        assert_eq!(
            window_in_direction(&rectangles, three, Direction::Up),
            Some(two)
        );

        layout = layout.remove(two).unwrap();
        assert_eq!(layout.windows(), vec![one, three]);
        layout = layout.remove(one).unwrap();
        assert_eq!(layout, Layout::Window(three));
        assert!(layout.remove(three).is_none());
    }

    #[test]
    fn resizes_the_nearest_matching_split_and_equalizes_the_tree() {
        let one = WindowId(1);
        let two = WindowId(2);
        let three = WindowId(3);
        let mut layout = Layout::Window(one);
        assert!(layout.split(one, two, SplitAxis::Columns));
        assert!(layout.split(two, three, SplitAxis::Rows));
        let area = Rect {
            rows: 10,
            columns: 21,
            ..Rect::default()
        };

        assert!(layout.resize(three, SplitAxis::Rows, ResizeAmount::Exact(3), area,));
        let rectangles = layout.rectangles(area);
        assert_eq!(rectangles[1].1.rows, 7);
        assert_eq!(rectangles[2].1.rows, 3);

        assert!(layout.resize(three, SplitAxis::Columns, ResizeAmount::Decrease(2), area,));
        let rectangles = layout.rectangles(area);
        assert_eq!(rectangles[0].1.columns, 12);
        assert_eq!(rectangles[1].1.columns, 8);
        assert_eq!(rectangles[2].1.columns, 8);

        assert!(layout.resize(three, SplitAxis::Columns, ResizeAmount::Exact(1), area,));
        assert_eq!(layout.rectangles(area)[2].1.columns, 4);

        layout.equalize();
        let rectangles = layout.rectangles(area);
        assert_eq!(rectangles[0].1.columns, 10);
        assert_eq!(rectangles[1].1.rows, 5);
        assert_eq!(rectangles[2].1.rows, 5);

        let four = WindowId(4);
        assert!(layout.split(three, four, SplitAxis::Columns));
        assert!(layout.resize(
            one,
            SplitAxis::Columns,
            ResizeAmount::Exact(usize::MAX),
            area,
        ));
        let rectangles = layout.rectangles(area);
        assert_eq!(rectangles[0].1.columns, 11);
        assert_eq!(rectangles[2].1.columns, 4);
        assert_eq!(rectangles[3].1.columns, 4);
    }
}
