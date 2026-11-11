//! Device-independent layout: data columns never shrink with the graphics.
pub const TEXT_WIDTH: i32 = 80;
pub const GAP: i32 = 4;
pub const GROUP_GAP: i32 = GAP * 3 / 2;
pub const GRIP: i32 = 10;
#[cfg(test)]
pub const LEADING: i32 = 8 + 56 + GROUP_GAP;
pub const MAX_GRAPHIC: i32 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Graphic {
    Bar(i32),
    Logo,
    None,
}

impl Graphic {
    pub fn width(self) -> i32 {
        match self {
            Self::Bar(width) => width,
            Self::Logo => 16,
            Self::None => 0,
        }
    }
}

#[derive(Debug)]
pub struct Layout {
    pub width: i32,
    pub graphics: [Graphic; 7],
    pub text_widths: [i32; 7],
    pub label_width: i32,
}

pub fn layout(requested: Option<i32>, enabled: [bool; 7], logos: [bool; 7]) -> Layout {
    layout_with_metrics(requested, enabled, logos, [TEXT_WIDTH; 7], 56)
}

pub fn layout_with_metrics(
    requested: Option<i32>,
    enabled: [bool; 7],
    logos: [bool; 7],
    text_widths: [i32; 7],
    label_width: i32,
) -> Layout {
    let count = enabled.iter().filter(|&&on| on).count().max(1) as i32;
    let text_total = enabled
        .iter()
        .zip(text_widths)
        .filter(|(on, _)| **on)
        .map(|(_, width)| width)
        .sum::<i32>();
    let fixed = 8 + label_width + GROUP_GAP + GRIP + text_total + (count - 1) * GROUP_GAP;
    let natural = fixed
        + enabled
            .iter()
            .zip(logos)
            .map(|(&on, logo)| {
                if !on {
                    0
                } else if logo {
                    16 + GAP
                } else {
                    44 + GAP
                }
            })
            .sum::<i32>();
    let budget = requested
        .unwrap_or(natural)
        .clamp(fixed, fixed + count * (MAX_GRAPHIC + GAP))
        - fixed;
    let logo_count = enabled
        .iter()
        .zip(logos)
        .filter(|(on, logo)| **on && *logo)
        .count() as i32;
    let bar_count = count - logo_count;
    let bars_fit = budget >= bar_count * 32 + logo_count * 20;
    let bar_width = if bar_count > 0 {
        (budget - logo_count * 20) / bar_count - GAP
    } else {
        0
    };
    let mut graphics = [Graphic::None; 7];
    for i in 0..7 {
        if enabled[i] {
            graphics[i] = if bars_fit && !logos[i] {
                Graphic::Bar(bar_width)
            } else if budget >= count * 20 {
                Graphic::Logo
            } else {
                Graphic::None
            };
        }
    }
    let width = fixed
        + graphics
            .iter()
            .map(|&g| {
                if g == Graphic::None {
                    0
                } else {
                    g.width() + GAP
                }
            })
            .sum::<i32>();
    Layout {
        width,
        graphics,
        text_widths,
        label_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measured_text_reclaims_padding_and_group_gap_is_one_and_a_half_inner_gaps() {
        assert_eq!(GROUP_GAP * 2, GAP * 3);
        let enabled = [true, true, false, false, false, false, false];
        let compact = layout_with_metrics(None, enabled, [false; 7], [18, 55, 0, 0, 0, 0, 0], 12);
        let fixed = layout(None, enabled, [false; 7]);
        assert_eq!(
            fixed.width - compact.width,
            (56 - 12) + (80 - 18) + (80 - 55)
        );
        assert_eq!(compact.text_widths[0], 18);
        assert_eq!(compact.graphics[0], Graphic::Bar(44));
        assert_eq!(compact.graphics[1], Graphic::Bar(44));
    }
    #[test]
    fn default_bar_choice_is_preserved_among_six_logo_choices() {
        for bar in 0..7 {
            let mut logos = [true; 7];
            logos[bar] = false;
            let result = layout(None, [true; 7], logos);
            assert_eq!(result.graphics[bar], Graphic::Bar(44));
            for i in 0..7 {
                if i != bar {
                    assert_eq!(result.graphics[i], Graphic::Logo);
                }
            }
        }
    }
    #[test]
    fn narrowing_is_monotonic_and_never_overlaps_fixed_data_columns() {
        for logos in [
            [false; 7],
            [true; 7],
            [true, false, true, false, true, false, true],
        ] {
            let enabled = [true; 7];
            let mut previous = 0;
            for requested in 0..2200 {
                let current = layout(Some(requested), enabled, logos);
                assert!(current.width >= previous);
                assert!(current.width >= LEADING + GRIP + 7 * TEXT_WIDTH + 6 * GROUP_GAP);
                previous = current.width;
            }
        }
    }
    #[test]
    fn resizing_preserves_data_columns_and_reaches_all_three_stages() {
        for count in 1..=7 {
            let mut enabled = [false; 7];
            enabled[..count].fill(true);
            let min = layout(Some(0), enabled, [false; 7]);
            assert_eq!(
                min.width,
                LEADING + GRIP + count as i32 * TEXT_WIDTH + (count as i32 - 1) * GROUP_GAP
            );
            assert!(min.graphics.iter().all(|&g| g == Graphic::None));
            let icons = layout(Some(min.width + count as i32 * 24), enabled, [false; 7]);
            assert!(icons.graphics[..count].iter().all(|&g| g == Graphic::Logo));
            let bars = layout(None, enabled, [false; 7]);
            assert!(bars.graphics[..count]
                .iter()
                .all(|g| matches!(g, Graphic::Bar(_))));
            assert!(bars.width > icons.width && icons.width > min.width);
        }
    }
    #[test]
    fn independent_logo_preferences_survive_width_changes() {
        let enabled = [true; 7];
        let logos = [false, true, false, true, false, true, false];
        let roomy = layout(Some(2000), enabled, logos);
        for i in 0..7 {
            assert_eq!(roomy.graphics[i] == Graphic::Logo, logos[i]);
        }
        assert!(layout(Some(-100), enabled, logos)
            .graphics
            .iter()
            .all(|&g| g == Graphic::None));
        assert_eq!(layout(Some(i32::MAX), enabled, logos).width, roomy.width);
    }
}
