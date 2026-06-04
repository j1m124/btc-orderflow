mod area_chart;
mod bar_chart;
mod candlestick_chart;
mod line_chart;
mod pie_chart;

pub use area_chart::AreaChart;
pub use bar_chart::BarChart;
pub use candlestick_chart::CandlestickChart;
pub use line_chart::LineChart;
pub use pie_chart::PieChart;

use gpui::{Hsla, SharedString, TextAlign, px};
use num_traits::ToPrimitive;

use crate::plot::{
    AxisText,
    scale::{Scale, ScaleBand, ScalePoint},
};

/// Build x-axis labels for point-based scales (`LineChart`, `AreaChart`).
///
/// Point scales place items at evenly spaced positions. The first label is
/// left-aligned, the last is right-aligned, and the rest are centered.
pub(crate) fn build_point_x_labels<T, X>(
    data: &[T],
    x_fn: &dyn Fn(&T) -> X,
    x_scale: &ScalePoint<X>,
    tick_margin: usize,
    color: Hsla,
) -> Vec<AxisText>
where
    X: PartialEq + Into<SharedString>,
{
    let data_len = data.len();
    data.iter()
        .enumerate()
        .filter_map(|(i, d)| {
            if (i + 1) % tick_margin != 0 {
                return None;
            }
            x_scale.tick(&x_fn(d)).map(|x_tick| {
                let align = match i {
                    0 if data_len == 1 => TextAlign::Center,
                    0 => TextAlign::Left,
                    i if i == data_len - 1 => TextAlign::Right,
                    _ => TextAlign::Center,
                };
                // Call x_fn again to get an owned value for the label text.
                AxisText::new(x_fn(d).into(), x_tick, color).align(align)
            })
        })
        .collect()
}

/// Pixels reserved on the right edge for y-axis price labels. Sized for the
/// widest expected label (e.g. "$1234.56") at the default text size.
///
/// Exposed publicly so callers that overlay interactive zones on the axis
/// (e.g. drag-to-scale handles) can size them to match.
pub fn nice_y_axis_gap() -> f32 {
    52.
}

/// Pick a "nice" step size near `raw_step`: 1, 2, or 5 × 10^k. Mirrors the
/// classic d3-scale algorithm — produces tick values that read cleanly and
/// stay stable as the data range drifts.
fn nice_step(raw_step: f64) -> f64 {
    if raw_step <= 0.0 || !raw_step.is_finite() {
        return 1.0;
    }
    let exp = raw_step.log10().floor();
    let pow10 = 10f64.powf(exp);
    let frac = raw_step / pow10;
    let nice = if frac < 1.5 {
        1.0
    } else if frac < 3.5 {
        2.0
    } else if frac < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * pow10
}

/// Format a price for y-axis labels. Picks decimals based on magnitude so
/// that wide-range and tight-range charts both stay legible.
fn format_price(value: f64) -> SharedString {
    let abs = value.abs();
    let decimals = if abs >= 100.0 {
        2
    } else if abs >= 1.0 {
        2
    } else if abs >= 0.01 {
        4
    } else {
        6
    };
    SharedString::from(format!("{:.*}", decimals, value))
}

/// Build evenly spaced price-axis labels at "nice" round values across the
/// data's y range. The pixel positions are computed by linear interpolation
/// between (`min_value` → `bottom_y`) and (`max_value` → `top_y`), so the
/// caller is responsible for matching the chart's actual y range.
pub(crate) fn build_y_price_labels<Y>(
    values: impl IntoIterator<Item = Y>,
    top_y: f32,
    bottom_y: f32,
    tick_count: usize,
    color: Hsla,
) -> Vec<AxisText>
where
    Y: Copy + ToPrimitive,
{
    let mut min: Option<f64> = None;
    let mut max: Option<f64> = None;
    for v in values {
        let Some(f) = v.to_f64() else { continue };
        if !f.is_finite() {
            continue;
        }
        min = Some(min.map_or(f, |m: f64| m.min(f)));
        max = Some(max.map_or(f, |m: f64| m.max(f)));
    }
    let (Some(min), Some(max)) = (min, max) else {
        return Vec::new();
    };
    if max <= min {
        return Vec::new();
    }
    let target = tick_count.max(2) as f64;
    let step = nice_step((max - min) / (target - 1.0));
    if step <= 0.0 {
        return Vec::new();
    }

    let span = max - min;
    let pixel_span = bottom_y - top_y;
    let to_pixel = |value: f64| -> f32 {
        let t = (value - min) / span;
        bottom_y - (t as f32) * pixel_span
    };

    let first = (min / step).ceil() * step;
    let mut out = Vec::new();
    let cap = (tick_count.max(2) * 4).max(10);
    let mut t = first;
    while t <= max + step * 0.0001 && out.len() < cap {
        let y_pixel = to_pixel(t);
        out.push(AxisText::new(format_price(t), px(y_pixel), color).align(TextAlign::Left));
        t += step;
    }
    out
}

/// Build axis labels for band-based scales (`BarChart`, `CandlestickChart`).
///
/// Band scales place items in evenly sized bands. The returned `tick`
/// coordinate is the centre of each band along the band axis; the caller
/// decides whether to feed the result to `PlotAxis::x_label` (vertical
/// charts) or `PlotAxis::y_label` (horizontal charts).
pub(crate) fn build_band_labels<T, X>(
    data: &[T],
    x_fn: &dyn Fn(&T) -> X,
    x_scale: &ScaleBand<X>,
    band_width: f32,
    tick_margin: usize,
    color: Hsla,
) -> Vec<AxisText>
where
    X: PartialEq + Into<SharedString>,
{
    data.iter()
        .enumerate()
        .filter_map(|(i, d)| {
            if (i + 1) % tick_margin != 0 {
                return None;
            }
            x_scale.tick(&x_fn(d)).map(|x_tick| {
                // Call x_fn again to get an owned value for the label text.
                AxisText::new(x_fn(d).into(), x_tick + band_width / 2., color)
                    .align(TextAlign::Center)
            })
        })
        .collect()
}
