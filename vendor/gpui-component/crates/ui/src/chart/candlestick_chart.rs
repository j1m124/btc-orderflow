use std::rc::Rc;

use gpui::{App, Bounds, Hsla, PathBuilder, Pixels, SharedString, Window, fill, px};
use gpui_component_macros::IntoPlot;
use num_traits::{Num, ToPrimitive};

use crate::{
    ActiveTheme,
    plot::{
        AXIS_GAP, AxisLabelSide, Grid, Plot, PlotAxis, origin_point,
        scale::{Scale, ScaleBand, ScaleLinear, Sealed},
    },
};

use super::{build_band_labels, build_y_price_labels, nice_y_axis_gap};

#[derive(IntoPlot)]
pub struct CandlestickChart<T, X, Y>
where
    T: 'static,
    X: PartialEq + Into<SharedString> + 'static,
    Y: Copy + PartialOrd + Num + ToPrimitive + Sealed + 'static,
{
    data: Vec<T>,
    x: Option<Rc<dyn Fn(&T) -> X>>,
    open: Option<Rc<dyn Fn(&T) -> Y>>,
    high: Option<Rc<dyn Fn(&T) -> Y>>,
    low: Option<Rc<dyn Fn(&T) -> Y>>,
    close: Option<Rc<dyn Fn(&T) -> Y>>,
    tick_margin: usize,
    body_width_ratio: f32,
    x_axis: bool,
    /// Show price ticks on the right edge. When enabled the chart reserves
    /// `nice_y_axis_gap()` pixels on the right for label text.
    y_axis: bool,
    /// Approximate number of price ticks to draw when `y_axis` is on; the
    /// actual count may differ slightly because tick values are snapped to
    /// nice round numbers.
    y_tick_count: usize,
    /// Optional explicit price-axis domain `(min, max)`. When set, the chart
    /// uses this range instead of auto-fitting to the visible data — letting
    /// callers lock the y view across renders for manual zoom/pan UX.
    y_domain: Option<(Y, Y)>,
    grid: bool,
}

impl<T, X, Y> CandlestickChart<T, X, Y>
where
    X: PartialEq + Into<SharedString> + 'static,
    Y: Copy + PartialOrd + Num + ToPrimitive + Sealed + 'static,
{
    pub fn new<I>(data: I) -> Self
    where
        I: IntoIterator<Item = T>,
    {
        Self {
            data: data.into_iter().collect(),
            x: None,
            open: None,
            high: None,
            low: None,
            close: None,
            tick_margin: 1,
            body_width_ratio: 0.8,
            x_axis: true,
            y_axis: false,
            y_tick_count: 5,
            y_domain: None,
            grid: true,
        }
    }

    pub fn x(mut self, x: impl Fn(&T) -> X + 'static) -> Self {
        self.x = Some(Rc::new(x));
        self
    }

    pub fn open(mut self, open: impl Fn(&T) -> Y + 'static) -> Self {
        self.open = Some(Rc::new(open));
        self
    }

    pub fn high(mut self, high: impl Fn(&T) -> Y + 'static) -> Self {
        self.high = Some(Rc::new(high));
        self
    }

    pub fn low(mut self, low: impl Fn(&T) -> Y + 'static) -> Self {
        self.low = Some(Rc::new(low));
        self
    }

    pub fn close(mut self, close: impl Fn(&T) -> Y + 'static) -> Self {
        self.close = Some(Rc::new(close));
        self
    }

    pub fn tick_margin(mut self, tick_margin: usize) -> Self {
        self.tick_margin = tick_margin;
        self
    }

    pub fn body_width_ratio(mut self, ratio: f32) -> Self {
        self.body_width_ratio = ratio;
        self
    }

    /// Show or hide the x-axis line and labels.
    ///
    /// Default is true.
    pub fn x_axis(mut self, x_axis: bool) -> Self {
        self.x_axis = x_axis;
        self
    }

    /// Show or hide the price (y-axis) tick labels on the right edge.
    ///
    /// Default is false — older callers that fill the full width keep working
    /// unchanged; opt in when you want price labels.
    pub fn y_axis(mut self, y_axis: bool) -> Self {
        self.y_axis = y_axis;
        self
    }

    /// Approximate number of price ticks to draw. Default is 5.
    pub fn y_tick_count(mut self, y_tick_count: usize) -> Self {
        self.y_tick_count = y_tick_count.max(2);
        self
    }

    /// Lock the price-axis domain to `(min, max)`. Pass `None` to fall back
    /// to the auto-fit behaviour (default).
    pub fn y_domain(mut self, domain: Option<(Y, Y)>) -> Self {
        self.y_domain = domain;
        self
    }

    pub fn grid(mut self, grid: bool) -> Self {
        self.grid = grid;
        self
    }
}

impl<T, X, Y> Plot for CandlestickChart<T, X, Y>
where
    X: PartialEq + Into<SharedString> + 'static,
    Y: Copy + PartialOrd + Num + ToPrimitive + Sealed + 'static,
{
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let (Some(x_fn), Some(open_fn), Some(high_fn), Some(low_fn), Some(close_fn)) = (
            self.x.as_ref(),
            self.open.as_ref(),
            self.high.as_ref(),
            self.low.as_ref(),
            self.close.as_ref(),
        ) else {
            return;
        };

        let total_width = bounds.size.width.as_f32();
        let y_axis_gap = if self.y_axis { nice_y_axis_gap() } else { 0. };
        let width = (total_width - y_axis_gap).max(0.);
        let axis_gap = if self.x_axis { AXIS_GAP } else { 0. };
        let height = bounds.size.height.as_f32() - axis_gap;

        // X scale
        let x = ScaleBand::new(self.data.iter().map(|v| x_fn(v)).collect(), vec![0., width])
            .padding_inner(0.4)
            .padding_outer(0.2);
        let band_width = x.band_width();

        // Y scale — explicit domain wins over auto-fit so callers can lock
        // the y view across renders for manual zoom/pan UX.
        let y = if let Some((min, max)) = self.y_domain {
            ScaleLinear::new(vec![min, max], vec![height, 10.])
        } else {
            let all_values: Vec<Y> = self
                .data
                .iter()
                .flat_map(|d| vec![high_fn(d), low_fn(d), open_fn(d), close_fn(d)])
                .collect();
            ScaleLinear::new(all_values, vec![height, 10.])
        };

        // Draw axes
        let mut axis = PlotAxis::new().stroke(cx.theme().border);
        if self.x_axis {
            let labels = build_band_labels(
                &self.data,
                x_fn.as_ref(),
                &x,
                band_width,
                self.tick_margin,
                cx.theme().muted_foreground,
            );
            axis = axis.x(height).x_label(labels);
        }
        if self.y_axis {
            // Top of the plot is at pixel y=10 (matching the y scale's range
            // upper bound above); bottom is at `height`. Use the explicit
            // y_domain when locked so the labels match what the chart paints.
            let y_label_ticks = if let Some((min, max)) = self.y_domain {
                build_y_price_labels(
                    [min, max],
                    10.,
                    height,
                    self.y_tick_count,
                    cx.theme().muted_foreground,
                )
            } else {
                let values = self.data.iter().flat_map(|d| {
                    [open_fn(d), high_fn(d), low_fn(d), close_fn(d)]
                });
                build_y_price_labels(
                    values,
                    10.,
                    height,
                    self.y_tick_count,
                    cx.theme().muted_foreground,
                )
            };
            axis = axis
                .y(width)
                .y_axis(false) // labels only — no vertical axis line
                .y_label_side(AxisLabelSide::End)
                .y_label(y_label_ticks);
        }
        axis.paint(&bounds, window, cx);

        // Draw grid
        if self.grid {
            Grid::new()
                .y((0..=3).map(|i| height * i as f32 / 4.0).collect())
                .stroke(cx.theme().border)
                .dash_array(&[px(4.), px(2.)])
                .paint(&bounds, window);
        }

        // Draw candlesticks
        let origin = bounds.origin;
        let x_fn = x_fn.clone();
        let open_fn = open_fn.clone();
        let high_fn = high_fn.clone();
        let low_fn = low_fn.clone();
        let close_fn = close_fn.clone();

        for d in &self.data {
            let x_tick = x.tick(&x_fn(d));
            let Some(x_tick) = x_tick else {
                continue;
            };

            // Get OHLC values for the current data point
            let open = open_fn(d);
            let high = high_fn(d);
            let low = low_fn(d);
            let close = close_fn(d);

            // Convert values to pixel coordinates
            let open_y = y.tick(&open);
            let high_y = y.tick(&high);
            let low_y = y.tick(&low);
            let close_y = y.tick(&close);

            let (Some(open_y), Some(high_y), Some(low_y), Some(close_y)) =
                (open_y, high_y, low_y, close_y)
            else {
                continue;
            };
            // When `y_domain` is locked tighter than the data, prices outside
            // the view would paint above/below the canvas. Clamp to the plot
            // band so partial candles still render but stay inside bounds;
            // skip candles that fall fully outside (every value at one edge).
            let top = 10.;
            let bot = height;
            let all_above = high_y < top && low_y < top;
            let all_below = high_y > bot && low_y > bot;
            if all_above || all_below {
                continue;
            }
            let clamp_y = |v: f32| v.clamp(top, bot);
            let open_y = clamp_y(open_y);
            let high_y = clamp_y(high_y);
            let low_y = clamp_y(low_y);
            let close_y = clamp_y(close_y);

            // Determine if bullish (close > open) or bearish (close < open)
            let is_bullish = close > open;
            let color: Hsla = if is_bullish {
                cx.theme().chart_bullish
            } else {
                cx.theme().chart_bearish
            };

            // Calculate candlestick body dimensions
            let center_x = x_tick + band_width / 2.;
            let body_width = band_width * self.body_width_ratio;
            let body_left = center_x - body_width / 2.;
            let body_right = center_x + body_width / 2.;

            // Draw wick (high to low line)
            let mut wick_builder = PathBuilder::stroke(px(1.));
            wick_builder.move_to(origin_point(px(center_x), px(high_y), origin));
            wick_builder.line_to(origin_point(px(center_x), px(low_y), origin));

            if let Ok(path) = wick_builder.build() {
                window.paint_path(path, color);
            }

            // Draw body (open to close rectangle)
            // For bullish: top is close, bottom is open
            // For bearish: top is open, bottom is close
            let (top, bottom) = if is_bullish {
                (close_y, open_y)
            } else {
                (open_y, close_y)
            };

            let body_bounds = Bounds::from_corners(
                origin_point(px(body_left), px(top), origin),
                origin_point(px(body_right), px(bottom), origin),
            );

            window.paint_quad(fill(body_bounds, color));
        }
    }
}
