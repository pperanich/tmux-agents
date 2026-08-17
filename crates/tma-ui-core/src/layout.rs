//! Where each live surface puts its list, as pure geometry.
//!
//! The draw and the mouse hit-test have to agree to the cell: a click that resolves to a row the
//! draw did not paint there selects the wrong pane, silently. So the split lives here once, both
//! sides read it, and neither owns a second copy. The fold keeps the frame area it last saw (from
//! `Resize`) and asks these functions the same question the draw asks.

use ratatui::layout::Rect;

use crate::watch::WatchLayout;

/// Split a frame into the body (fills) and the one-line footer under it, the shape both surfaces
/// share. A frame too short for both gives the footer nothing.
pub fn body_and_footer(area: Rect) -> (Rect, Rect) {
    let footer_h = u16::from(area.height > 0);
    let body_h = area.height.saturating_sub(footer_h);
    (
        Rect {
            height: body_h,
            ..area
        },
        Rect {
            y: area.y + body_h,
            height: footer_h,
            ..area
        },
    )
}

/// A list widget's placement: its rect and whether a border eats the outer ring of cells. Hit-test
/// and viewport both come from here, so a bordered and a borderless list are the same code path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListGeom {
    pub rect: Rect,
    /// The bordered list arms (`agents (N)`); the table arm draws its rows borderless under a
    /// separate header line.
    pub bordered: bool,
}

impl ListGeom {
    /// The cells the rows themselves occupy (the rect minus the border ring).
    pub fn interior(&self) -> Rect {
        if !self.bordered {
            return self.rect;
        }
        Rect {
            x: self.rect.x.saturating_add(1),
            y: self.rect.y.saturating_add(1),
            width: self.rect.width.saturating_sub(2),
            height: self.rect.height.saturating_sub(2),
        }
    }

    /// How many rows are visible at once — the scroll window the fold keeps its selection inside.
    pub fn viewport(&self) -> usize {
        self.interior().height as usize
    }

    /// The draw index (the position in the item list the draw builds, group headers included) under
    /// `(col, row)`, or `None` when the point is outside the list's interior. `scroll` is the
    /// fold's own first-visible index, which is why both sides must read it from the same place.
    pub fn index_at(&self, scroll: usize, col: u16, row: u16) -> Option<usize> {
        let inner = self.interior();
        let hit = col >= inner.x
            && col < inner.x.saturating_add(inner.width)
            && row >= inner.y
            && row < inner.y.saturating_add(inner.height);
        hit.then(|| scroll + (row - inner.y) as usize)
    }
}

/// The `watch` sidebar's frame: the list, the preview beside it (wide arm only), the table's header
/// line (table arm only), and the footer. Mirrors `watch::draw` exactly — it is what `draw` lays
/// out from.
#[derive(Clone, Copy, Debug)]
pub struct WatchGeom {
    pub list: ListGeom,
    /// The live ANSI preview beside the list, `None` in the two arms that show none.
    pub preview: Option<Rect>,
    /// The table arm's one-line column header, above its borderless rows.
    pub table_header: Option<Rect>,
    pub footer: Rect,
}

/// The list width the wide arm gives the sidebar: the compact row is designed for ~32 columns, so
/// it stays fixed and the preview takes whatever the terminal adds.
const WIDE_LIST_W: u16 = 34;

/// Lay out the sidebar for `area` under `layout`.
pub fn watch_geom(area: Rect, layout: WatchLayout) -> WatchGeom {
    let (body, footer) = body_and_footer(area);
    match layout {
        WatchLayout::ListOnly => WatchGeom {
            list: ListGeom {
                rect: body,
                bordered: true,
            },
            preview: None,
            table_header: None,
            footer,
        },
        WatchLayout::ListAndPreview => {
            let list_w = WIDE_LIST_W.min(body.width);
            WatchGeom {
                list: ListGeom {
                    rect: Rect {
                        width: list_w,
                        ..body
                    },
                    bordered: true,
                },
                preview: Some(Rect {
                    x: body.x + list_w,
                    width: body.width - list_w,
                    ..body
                }),
                table_header: None,
                footer,
            }
        }
        WatchLayout::Table => {
            let header_h = u16::from(body.height > 0);
            WatchGeom {
                list: ListGeom {
                    rect: Rect {
                        y: body.y + header_h,
                        height: body.height - header_h,
                        ..body
                    },
                    bordered: false,
                },
                preview: None,
                table_header: Some(Rect {
                    height: header_h,
                    ..body
                }),
                footer,
            }
        }
    }
}

/// The picker popup's frame: the list, the preview beside it when the popup is wide enough, and the
/// query/footer line. Mirrors `picker::draw`.
#[derive(Clone, Copy, Debug)]
pub struct PickerGeom {
    pub list: ListGeom,
    pub preview: Option<Rect>,
    pub footer: Rect,
}

/// Lay out the picker for `area`. `preview_visible` is the fold's own decision (the width gate), so
/// the hit-test and the draw cannot disagree about which half the click landed in.
pub fn picker_geom(area: Rect, preview_visible: bool) -> PickerGeom {
    let (body, footer) = body_and_footer(area);
    if !preview_visible {
        return PickerGeom {
            list: ListGeom {
                rect: body,
                bordered: true,
            },
            preview: None,
            footer,
        };
    }
    // 55/45, the split `picker::draw` has always used.
    let list_w = body.width * 55 / 100;
    PickerGeom {
        list: ListGeom {
            rect: Rect {
                width: list_w,
                ..body
            },
            bordered: true,
        },
        preview: Some(Rect {
            x: body.x + list_w,
            width: body.width - list_w,
            ..body
        }),
        footer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn the_footer_takes_one_line_off_the_body() {
        let (body, footer) = body_and_footer(area(40, 10));
        assert_eq!((body.height, footer.y, footer.height), (9, 9, 1));
        // A zero-height frame has nothing to give either.
        let (body, footer) = body_and_footer(area(40, 0));
        assert_eq!((body.height, footer.height), (0, 0));
    }

    #[test]
    fn a_bordered_list_hit_tests_inside_its_border_only() {
        // 40x9 list at the origin: rows live on screen rows 1..=7.
        let geom = ListGeom {
            rect: area(40, 9),
            bordered: true,
        };
        assert_eq!(geom.viewport(), 7);
        assert_eq!(geom.index_at(0, 5, 1), Some(0), "first interior row");
        assert_eq!(geom.index_at(0, 5, 7), Some(6), "last interior row");
        assert_eq!(geom.index_at(0, 5, 0), None, "the top border is not a row");
        assert_eq!(geom.index_at(0, 5, 8), None, "nor the bottom");
        assert_eq!(geom.index_at(0, 0, 3), None, "nor the left border");
        assert_eq!(geom.index_at(0, 39, 3), None, "nor the right");
        // Scrolled: the same cell is a later item.
        assert_eq!(geom.index_at(4, 5, 1), Some(4));
    }

    #[test]
    fn a_borderless_list_uses_every_cell_it_has() {
        let geom = ListGeom {
            rect: Rect {
                x: 0,
                y: 3,
                width: 20,
                height: 4,
            },
            bordered: false,
        };
        assert_eq!(geom.viewport(), 4);
        assert_eq!(geom.index_at(0, 0, 3), Some(0), "its own first row");
        assert_eq!(geom.index_at(0, 19, 6), Some(3));
        assert_eq!(geom.index_at(0, 0, 2), None, "above it is the table header");
    }

    #[test]
    fn watch_arms_place_the_list_where_each_one_draws_it() {
        let narrow = watch_geom(area(32, 12), WatchLayout::ListOnly);
        assert_eq!(narrow.list.rect, area(32, 11));
        assert!(narrow.preview.is_none() && narrow.table_header.is_none());

        let wide = watch_geom(area(100, 12), WatchLayout::ListAndPreview);
        assert_eq!(wide.list.rect.width, WIDE_LIST_W);
        assert_eq!(wide.preview.unwrap().x, WIDE_LIST_W);
        assert_eq!(wide.preview.unwrap().width, 100 - WIDE_LIST_W);

        let table = watch_geom(area(100, 12), WatchLayout::Table);
        assert_eq!(table.table_header.unwrap().height, 1);
        assert_eq!(table.list.rect.y, 1, "rows start under the column header");
        assert!(!table.list.bordered);
        assert_eq!(table.list.viewport(), 10, "12 - footer - header");
    }

    /// A pane narrower than the fixed list width must not produce a negative-width preview; the
    /// list simply takes what there is.
    #[test]
    fn a_pane_narrower_than_the_wide_list_still_lays_out() {
        let g = watch_geom(area(20, 8), WatchLayout::ListAndPreview);
        assert_eq!(g.list.rect.width, 20);
        assert_eq!(g.preview.unwrap().width, 0);
    }

    #[test]
    fn the_picker_splits_55_45_only_with_a_preview() {
        let full = picker_geom(area(60, 10), false);
        assert_eq!(full.list.rect.width, 60);
        assert!(full.preview.is_none());

        let split = picker_geom(area(100, 10), true);
        assert_eq!(split.list.rect.width, 55);
        assert_eq!(split.preview.unwrap().x, 55);
        assert_eq!(split.preview.unwrap().width, 45);
    }
}
