// Copyright 2023 Google LLC
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ffi;
use {
    peniko::{
        kurbo::{Affine, Cap, Join, PathEl, Point, Stroke},
        Brush, Color, Fill, Mix,
    },
    std::pin::Pin,
    vello_encoding::{
        BumpEstimator, Encoding as VelloEncoding, PathEncoder, RenderConfig, Transform,
    },
};

pub(crate) struct Encoding {
    encoding: VelloEncoding,
    estimator: BumpEstimator,
}

pub(crate) fn new_encoding() -> Box<Encoding> {
    Box::new(Encoding::new())
}

impl Encoding {
    fn new() -> Encoding {
        // An encoding blob that doesn't represent a scene fragment (i.e. a reused blob that is
        // appended to a root encoding), then we need to initialize the transform and linewidth
        // streams with first entries (an identity transform and -1 linewidth value). Resetting
        // the encoding as non-fragment achieves this.
        let mut encoding = VelloEncoding::new();
        encoding.reset();
        Encoding { encoding, estimator: BumpEstimator::new(), }
    }

    pub fn is_empty(&self) -> bool {
        self.encoding.is_empty()
    }

    pub fn reset(&mut self) {
        self.encoding.reset();
        self.estimator.reset();
    }

    pub fn fill(
        &mut self,
        style: ffi::Fill,
        transform: ffi::Affine,
        brush: &ffi::Brush,
        path_iter: Pin<&mut ffi::PathIterator>,
    ) {
        let t = Transform::from_kurbo(&transform.into());
        self.encoding.encode_transform(t);
        self.encoding.encode_fill_style(style.into());
        if self.encode_path(path_iter, &t, None) {
            self.encoding.encode_brush(&Brush::from(brush), 1.0)
        }
    }

    pub fn stroke(
        &mut self,
        style: &ffi::Stroke,
        transform: ffi::Affine,
        brush: &ffi::Brush,
        path_iter: Pin<&mut ffi::PathIterator>,
    ) {
        let t = Transform::from_kurbo(&transform.into());
        self.encoding.encode_transform(t);

        // TODO: process any dash pattern here using kurbo's dash expander unless Graphite
        // handles dashing already.
        let stroke = style.into();
        self.encoding.encode_stroke_style(&stroke);
        if self.encode_path(path_iter, &t, Some(&stroke)) {
            self.encoding.encode_brush(&Brush::from(brush), 1.0);
        }
    }

    pub fn begin_clip(&mut self, transform: ffi::Affine, path_iter: Pin<&mut ffi::PathIterator>) {
        let t = Transform::from_kurbo(&transform.into());
        self.encoding.encode_transform(t);
        self.encoding.encode_fill_style(Fill::NonZero);
        self.encode_path(path_iter, &t, None);
        self.encoding.encode_begin_clip(Mix::Clip.into(), /*alpha=*/ 1.0);
    }

    pub fn end_clip(&mut self) {
        self.encoding.encode_end_clip();
    }

    pub fn append(&mut self, other: &Encoding) {
        self.encoding.append(&other.encoding, &None);
        self.estimator.append(&other.estimator, None);
    }

    pub fn prepare_render(
        &self,
        width: u32,
        height: u32,
        background: &ffi::Color,
    ) -> Box<RenderConfiguration> {
        let mut packed_scene = Vec::new();
        let layout = vello_encoding::resolve_solid_paths_only(&self.encoding, &mut packed_scene);
        let mut config = RenderConfig::new(&layout, width, height, &background.into());

        let bump_estimate = self.estimator.tally(None);
        //println!("bump: {bump_estimate}");
        config.buffer_sizes.bin_data = bump_estimate.binning;
        config.buffer_sizes.seg_counts = bump_estimate.seg_counts;
        config.buffer_sizes.segments = bump_estimate.segments;
        config.buffer_sizes.lines = bump_estimate.lines;
        config.gpu.binning_size = bump_estimate.binning.len();
        config.gpu.seg_counts_size = bump_estimate.seg_counts.len();
        config.gpu.segments_size = bump_estimate.segments.len();
        config.gpu.lines_size = bump_estimate.lines.len();

        Box::new(RenderConfiguration {
            packed_scene,
            config,
        })
    }

    fn encode_path(
        &mut self,
        iter: Pin<&mut ffi::PathIterator>,
        transform: &Transform,
        stroke: Option<&Stroke>,
    ) -> bool {
        let mut encoder = self.encoding.encode_path(/*is_fill=*/ stroke.is_none());

        // Wrap the input iterator inside a custom iterator, so that the path gets
        // encoded as the estimator runs through it.
        let path = IterablePathEncoder { iter, encoder: &mut encoder };
        self.estimator.count_path(path, transform, stroke);
        encoder.finish(/*insert_path_marker=*/ true) != 0
    }
}

// This is path element iterator that encodes path elements as it gets polled.
struct IterablePathEncoder<'a, 'b> {
    iter: Pin<&'a mut ffi::PathIterator>,
    encoder: &'a mut PathEncoder<'b>,
}

impl Iterator for IterablePathEncoder<'_, '_> {
    type Item = PathEl;

    fn next(&mut self) -> Option<Self::Item> {
        let mut path_el = ffi::PathElement::default();
        if !unsafe { self.iter.as_mut().next_element(&mut path_el) } {
            return None;
        }
        Some(match path_el.verb {
            ffi::PathVerb::MoveTo => {
                let p = &path_el.points[0];
                self.encoder.move_to(p.x, p.y);
                PathEl::MoveTo(p.into())
            }
            ffi::PathVerb::LineTo => {
                let p = &path_el.points[1];
                self.encoder.line_to(p.x, p.y);
                PathEl::LineTo(p.into())
            }
            ffi::PathVerb::QuadTo => {
                let p0 = &path_el.points[1];
                let p1 = &path_el.points[2];
                self.encoder.quad_to(p0.x, p0.y, p1.x, p1.y);
                PathEl::QuadTo(p0.into(), p1.into())
            }
            ffi::PathVerb::CurveTo => {
                let p0 = &path_el.points[1];
                let p1 = &path_el.points[2];
                let p2 = &path_el.points[3];
                self.encoder.cubic_to(p0.x, p0.y, p1.x, p1.y, p2.x, p2.y);
                PathEl::CurveTo(p0.into(), p1.into(), p2.into())
            }
            ffi::PathVerb::Close => {
                self.encoder.close();
                PathEl::ClosePath
            }
            _ => panic!("invalid path verb"),
        })
    }
}

pub(crate) struct RenderConfiguration {
    packed_scene: Vec<u8>,
    config: RenderConfig,
}

impl RenderConfiguration {
    pub fn config_uniform_buffer_size(self: &RenderConfiguration) -> usize {
        std::mem::size_of::<vello_encoding::ConfigUniform>()
    }

    pub fn scene_buffer_size(self: &RenderConfiguration) -> usize {
        self.packed_scene.len()
    }

    pub fn write_config_uniform_buffer(self: &RenderConfiguration, out_buffer: &mut [u8]) -> bool {
        let bytes = bytemuck::bytes_of(&self.config.gpu);
        if out_buffer.len() < bytes.len() {
            return false;
        }
        out_buffer.copy_from_slice(bytes);
        true
    }

    pub fn write_scene_buffer(self: &RenderConfiguration, out_buffer: &mut [u8]) -> bool {
        if out_buffer.len() < self.packed_scene.len() {
            return false;
        }
        out_buffer.copy_from_slice(&self.packed_scene);
        true
    }

    pub fn workgroup_counts(self: &RenderConfiguration) -> ffi::DispatchInfo {
        (&self.config.workgroup_counts).into()
    }

    pub fn buffer_sizes(self: &RenderConfiguration) -> ffi::BufferSizes {
        (&self.config.buffer_sizes).into()
    }
}

impl Iterator for Pin<&mut ffi::PathIterator> {
    type Item = PathEl;

    fn next(&mut self) -> Option<PathEl> {
        let mut path_el = ffi::PathElement::default();
        if !unsafe { self.as_mut().next_element(&mut path_el) } {
            return None;
        }
        Some(match path_el.verb {
            ffi::PathVerb::MoveTo => {
                let p = &path_el.points[0];
                PathEl::MoveTo(p.into())
            }
            ffi::PathVerb::LineTo => {
                let p = &path_el.points[1];
                PathEl::LineTo(p.into())
            }
            ffi::PathVerb::QuadTo => {
                let p0 = &path_el.points[1];
                let p1 = &path_el.points[2];
                PathEl::QuadTo(p0.into(), p1.into())
            }
            ffi::PathVerb::CurveTo => {
                let p0 = &path_el.points[1];
                let p1 = &path_el.points[2];
                let p2 = &path_el.points[3];
                PathEl::CurveTo(p0.into(), p1.into(), p2.into())
            }
            ffi::PathVerb::Close => PathEl::ClosePath,
            _ => panic!("invalid path verb"),
        })
    }
}

impl From<&ffi::Point> for Point {
    fn from(src: &ffi::Point) -> Self {
        Self::new(src.x.into(), src.y.into())
    }
}

impl Default for ffi::PathVerb {
    fn default() -> Self {
        Self::MoveTo
    }
}

impl From<ffi::Affine> for Affine {
    fn from(src: ffi::Affine) -> Self {
        Self::new([
            src.matrix[0] as f64,
            src.matrix[1] as f64,
            src.matrix[2] as f64,
            src.matrix[3] as f64,
            src.matrix[4] as f64,
            src.matrix[5] as f64,
        ])
    }
}

impl From<&ffi::Color> for Color {
    fn from(src: &ffi::Color) -> Self {
        Self {
            r: src.r,
            g: src.g,
            b: src.b,
            a: src.a,
        }
    }
}

impl From<&ffi::Brush> for Brush {
    fn from(src: &ffi::Brush) -> Self {
        match src.kind {
            ffi::BrushKind::Solid => Brush::Solid(Color::from(&src.data.solid)),
            _ => panic!("invalid brush kind"),
        }
    }
}

impl From<ffi::Fill> for Fill {
    fn from(src: ffi::Fill) -> Self {
        match src {
            ffi::Fill::NonZero => Self::NonZero,
            ffi::Fill::EvenOdd => Self::EvenOdd,
            _ => panic!("invalid fill type"),
        }
    }
}

impl From<&ffi::Stroke> for Stroke {
    fn from(src: &ffi::Stroke) -> Self {
        let cap = match src.cap {
            ffi::CapStyle::Butt => Cap::Butt,
            ffi::CapStyle::Square => Cap::Square,
            ffi::CapStyle::Round => Cap::Round,
            _ => panic!("invalid cap style"),
        };
        Self {
            width: src.width as f64,
            join: match src.join {
                ffi::JoinStyle::Bevel => Join::Bevel,
                ffi::JoinStyle::Miter => Join::Miter,
                ffi::JoinStyle::Round => Join::Round,
                _ => panic!("invalid join style"),
            },
            miter_limit: src.miter_limit as f64,
            start_cap: cap,
            end_cap: cap,
            // Skia expands a dash effect by transforming the encoded path, so don't need to handle
            // that here.
            dash_pattern: Default::default(),
            dash_offset: 0.,
        }
    }
}

impl From<&vello_encoding::WorkgroupSize> for ffi::WorkgroupSize {
    fn from(src: &vello_encoding::WorkgroupSize) -> Self {
        Self {
            x: src.0,
            y: src.1,
            z: src.2,
        }
    }
}

impl From<&vello_encoding::WorkgroupCounts> for ffi::DispatchInfo {
    fn from(src: &vello_encoding::WorkgroupCounts) -> Self {
        Self {
            use_large_path_scan: src.use_large_path_scan,
            path_reduce: (&src.path_reduce).into(),
            path_reduce2: (&src.path_reduce2).into(),
            path_scan1: (&src.path_scan1).into(),
            path_scan: (&src.path_scan).into(),
            bbox_clear: (&src.bbox_clear).into(),
            flatten: (&src.flatten).into(),
            draw_reduce: (&src.draw_reduce).into(),
            draw_leaf: (&src.draw_leaf).into(),
            clip_reduce: (&src.clip_reduce).into(),
            clip_leaf: (&src.clip_leaf).into(),
            binning: (&src.binning).into(),
            tile_alloc: (&src.tile_alloc).into(),
            path_count_setup: (&src.path_count_setup).into(),
            backdrop: (&src.backdrop).into(),
            coarse: (&src.coarse).into(),
            path_tiling_setup: (&src.path_tiling_setup).into(),
            fine: (&src.fine).into(),
        }
    }
}

impl From<&vello_encoding::BufferSizes> for ffi::BufferSizes {
    fn from(src: &vello_encoding::BufferSizes) -> Self {
        Self {
            path_reduced: src.path_reduced.size_in_bytes(),
            path_reduced2: src.path_reduced2.size_in_bytes(),
            path_reduced_scan: src.path_reduced_scan.size_in_bytes(),
            path_monoids: src.path_monoids.size_in_bytes(),
            path_bboxes: src.path_bboxes.size_in_bytes(),
            draw_reduced: src.draw_reduced.size_in_bytes(),
            draw_monoids: src.draw_monoids.size_in_bytes(),
            info: src.info.size_in_bytes(),
            clip_inps: src.clip_inps.size_in_bytes(),
            clip_els: src.clip_els.size_in_bytes(),
            clip_bics: src.clip_bics.size_in_bytes(),
            clip_bboxes: src.clip_bboxes.size_in_bytes(),
            draw_bboxes: src.draw_bboxes.size_in_bytes(),
            bump_alloc: src.bump_alloc.size_in_bytes(),
            indirect_count: src.indirect_count.size_in_bytes(),
            bin_headers: src.bin_headers.size_in_bytes(),
            paths: src.paths.size_in_bytes(),
            lines: src.lines.size_in_bytes(),
            bin_data: src.bin_data.size_in_bytes(),
            tiles: src.tiles.size_in_bytes(),
            seg_counts: src.seg_counts.size_in_bytes(),
            segments: src.segments.size_in_bytes(),
            ptcl: src.ptcl.size_in_bytes(),
        }
    }
}
