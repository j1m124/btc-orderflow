use std::rc::Rc;

use gpui::{App, Bounds, Hsla, Pixels, SharedString, Window, px};
use gpui_component_macros::IntoPlot;
use num_traits::{Num, ToPrimitive};

use crate::{
    ActiveTheme,
    plot::{
        AXIS_GAP, AxisLabelSide, Grid, Plot, PlotAxis, StrokeStyle,
        scale::{Scale, ScaleLinear, ScalePoint, Sealed},
        shape::Line,
    },
};

use super::{build_point_x_labels, build_y_price_labels, nice_y_axis_gap};

#[derive(IntoPlot)]
pub struct LineChart<T, X, Y>
where
    T: 'static,
    X: PartialEq + Into<SharedString> + 'static,
    Y: Copy + PartialOrd + Num + ToPrimitive + Sealed + 'static,
{
    data: Vec<T>,
    x: Option<Rc<dyn Fn(&T) -> X>>,
    y: Option<Rc<dyn Fn(&T) -> Y>>,
    stroke: Option<Hsla>,
    stroke_style: StrokeStyle,
    dot: bool,
    tick_margin: usize,
    x_axis: bool,
    y_axis: bool,
    y_tick_count: usize,
    grid: bool,
}

impl<T, X, Y> LineChart<T, X, Y>
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
            stroke: None,
            stroke_style: Default::default(),
            dot: false,
            x: None,
            y: None,
            tick_margin: 1,
            x_axis: true,
            y_axis: false,
            y_tick_count: 5,
            grid: true,
        }
    }

    pub fn x(mut self, x: impl Fn(&T) -> X + 'static) -> Self {
        self.x = Some(Rc::new(x));
        self
    }

    pub fn y(mut self, y: impl Fn(&T) -> Y + 'static) -> Self {
        self.y = Some(Rc::new(y));
        self
    }

    pub fn stroke(mut self, stroke: impl Into<Hsla>) -> Self {
        self.stroke = Some(stroke.into());
        self
    }

    pub fn natural(mut self) -> Self {
        self.stroke_style = StrokeStyle::Natural;
        self
    }

    pub fn linear(mut self) -> Self {
        self.stroke_style = StrokeStyle::Linear;
        self
    }

    pub fn step_after(mut self) -> Self {
        self.stroke_style = StrokeStyle::StepAfter;
        self
    }

    pub fn dot(mut self) -> Self {
        self.dot = true;
        self
    }

    pub fn tick_margin(mut self, tick_margin: usize) -> Self {
        self.tick_margin = tick_margin;
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
    /// Default is false.
    pub fn y_axis(mut self, y_axis: bool) -> Self {
        self.y_axis = y_axis;
        self
    }

    /// Approximate number of price ticks to draw. Default is 5.
    pub fn y_tick_count(mut self, y_tick_count: usize) -> Self {
        self.y_tick_count = y_tick_count.max(2);
        self
    }

    pub fn grid(mut self, grid: bool) -> Self {
        self.grid = grid;
        self
    }
}

impl<T, X, Y> Plot for LineChart<T, X, Y>
where
    X: PartialEq + Into<SharedString> + 'static,
    Y: Copy + PartialOrd + Num + ToPrimitive + Sealed + 'static,
{
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let (Some(x_fn), Some(y_fn)) = (self.x.as_ref(), self.y.as_ref()) else {
            return;
        };

        let total_width = bounds.size.width.as_f32();
        let y_axis_gap = if self.y_axis { nice_y_axis_gap() } else { 0. };
        let width = (total_width - y_axis_gap).max(0.);
        let axis_gap = if self.x_axis { AXIS_GAP } else { 0. };
        let height = bounds.size.height.as_f32() - axis_gap;

        // X scale
        let x = ScalePoint::new(self.data.iter().map(|v| x_fn(v)).collect(), vec![0., width]);

        // Y scale, ensure start from 0.
        let y = ScaleLinear::new(
            self.data
                .iter()
                .map(|v| y_fn(v))
                .chain(Some(Y::zero()))
                .collect(),
            vec![height, 10.],
        );

        // Draw axes
        let mut axis = PlotAxis::new().stroke(cx.theme().border);
        if self.x_axis {
            let labels = build_point_x_labels(
                &self.data,
                x_fn.as_ref(),
                &x,
                self.tick_margin,
                cx.theme().muted_foreground,
            );
            axis = axis.x(height).x_label(labels);
        }
        if self.y_axis {
            // ScaleLinear above includes Y::zero() in the domain so the line
            // visually anchors to a 0-baseline. We mirror that here so the
            // labels match what the chart actually plots.
            let values = self
                .data
                .iter()
                .map(|d| y_fn(d))
                .chain(Some(Y::zero()));
            let y_label_ticks = build_y_price_labels(
                values,
                10.,
                height,
                self.y_tick_count,
                cx.theme().muted_foreground,
            );
            axis = axis
                .y(width)
                .y_axis(false)
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

        // Draw line
        let stroke = self.stroke.unwrap_or(cx.theme().chart_2);
        let x_fn = x_fn.clone();
        let y_fn = y_fn.clone();
        let mut line = Line::new()
            .data(&self.data)
            .x(move |d| x.tick(&x_fn(d)))
            .y(move |d| y.tick(&y_fn(d)))
            .stroke(stroke)
            .stroke_style(self.stroke_style)
            .stroke_width(2.);

        if self.dot {
            line = line.dot().dot_size(8.).dot_fill_color(stroke);
        }

        line.paint(&bounds, window);
    }
}
