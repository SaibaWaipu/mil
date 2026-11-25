//! An [`anyrender::PaintScene`] implementation which records only the drawing
//! commands that can be represented in a terminal.
//!
//! Glyphs are deliberately *not* recorded: `draw_glyphs` delivers opaque font
//! glyph ids, and a terminal needs the original source text. Text is recovered
//! separately by walking the DOM. This scene keeps solid-colour fills (which
//! become cell backgrounds) and raster images (which become out-of-band PNGs).

use anyrender::{Glyph, NormalizedCoord, PaintRef, PaintScene};
use kurbo::{Affine, Rect, Shape, Stroke};
use peniko::{BlendMode, Color, Fill, FontData, ImageBrushRef, StyleRef};

/// A solid-colour rectangle recorded from a `fill` command.
///
/// Coordinates and size are already transformed into device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordedFill {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub color: Color,
}

/// A raster image recorded from an image `fill` (or `draw_image`) command.
///
/// `pixels` is a copy of the paint image data in its original format; the DOM
/// layer converts it to RGBA8 and PNG-encodes it before placing it on a surface.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedImage {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub pixels: Vec<u8>,
    pub image_width: u32,
    pub image_height: u32,
    pub format: peniko::ImageFormat,
    pub alpha_type: peniko::ImageAlphaType,
    pub sampler_alpha: f32,
}

/// Collects terminal-representable paint commands issued by `blitz-paint`.
#[derive(Debug, Clone, Default)]
pub struct TerminalScene {
    fills: Vec<RecordedFill>,
    images: Vec<RecordedImage>,
    clips: Vec<Option<Rect>>,
}

impl TerminalScene {
    /// Create an empty scene.
    pub fn new() -> Self {
        Self::default()
    }

    /// Solid fills recorded so far, in paint order.
    pub fn fills(&self) -> &[RecordedFill] {
        &self.fills
    }

    /// Images recorded so far, in paint order.
    pub fn images(&self) -> &[RecordedImage] {
        &self.images
    }

    /// Discard all recorded commands and clip layers.
    pub fn clear(&mut self) {
        self.fills.clear();
        self.images.clear();
        self.clips.clear();
    }

    /// Intersection of all active clip rectangles.
    fn current_clip(&self) -> Option<Rect> {
        self.clips
            .iter()
            .flatten()
            .copied()
            .reduce(|acc, rect| acc.intersect(rect))
            .filter(|rect| rect.width() > 0.0 && rect.height() > 0.0)
    }

    fn clipped(&self, rect: Rect) -> Option<Rect> {
        let rect = match self.current_clip() {
            Some(clip) => rect.intersect(clip),
            None => rect,
        };
        (rect.width() > 0.0 && rect.height() > 0.0).then_some(rect)
    }

    fn record_image(&mut self, rect: Rect, brush: ImageBrushRef<'_>) {
        let image = brush.image;
        let expected = image
            .format
            .size_in_bytes(image.width, image.height)
            .unwrap_or(0);
        let pixels = image.data.data();
        if pixels.len() < expected || image.width == 0 || image.height == 0 {
            return;
        }
        self.images.push(RecordedImage {
            x: rect.x0,
            y: rect.y0,
            width: rect.width(),
            height: rect.height(),
            pixels: pixels[..expected].to_vec(),
            image_width: image.width,
            image_height: image.height,
            format: image.format,
            alpha_type: image.alpha_type,
            sampler_alpha: brush.sampler.alpha,
        });
    }
}

impl PaintScene for TerminalScene {
    fn reset(&mut self) {
        self.clear();
    }

    fn push_layer(
        &mut self,
        _blend: impl Into<BlendMode>,
        _alpha: f32,
        transform: Affine,
        clip: &impl Shape,
    ) {
        let rect = transform.transform_rect_bbox(clip.bounding_box());
        self.clips.push(Some(rect));
    }

    fn pop_layer(&mut self) {
        self.clips.pop();
    }

    fn stroke<'a>(
        &mut self,
        _style: &Stroke,
        _transform: Affine,
        _brush: impl Into<PaintRef<'a>>,
        _brush_transform: Option<Affine>,
        _shape: &impl Shape,
    ) {
        // Strokes (borders, text decorations) are approximated by the text
        // attributes on cells, so emitting rectangles here would only add noise.
    }

    fn fill<'a>(
        &mut self,
        _style: Fill,
        transform: Affine,
        brush: impl Into<PaintRef<'a>>,
        _brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        let Some(rect) = self.clipped(transform.transform_rect_bbox(shape.bounding_box())) else {
            return;
        };
        match brush.into() {
            PaintRef::Solid(color) => {
                if color.components[3] <= f32::EPSILON {
                    return;
                }
                self.fills.push(RecordedFill {
                    x: rect.x0,
                    y: rect.y0,
                    width: rect.width(),
                    height: rect.height(),
                    color,
                });
            }
            PaintRef::Image(brush) => self.record_image(rect, brush),
            // Gradients and custom paints cannot be shown in a text grid.
            _ => {}
        }
    }

    fn draw_glyphs<'a, 's: 'a>(
        &'s mut self,
        _font: &'a FontData,
        _font_size: f32,
        _hint: bool,
        _normalized_coords: &'a [NormalizedCoord],
        _style: impl Into<StyleRef<'a>>,
        _brush: impl Into<PaintRef<'a>>,
        _brush_alpha: f32,
        _transform: Affine,
        _glyph_transform: Option<Affine>,
        _glyphs: impl Iterator<Item = Glyph>,
    ) {
        // Glyph ids cannot be mapped back to source text. See module docs.
    }

    fn draw_box_shadow(
        &mut self,
        _transform: Affine,
        _rect: Rect,
        _brush: Color,
        _radius: f64,
        _std_dev: f64,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peniko::{
        ImageAlphaType, ImageData, ImageFormat, ImageSampler, linebender_resource_handle::Blob,
    };

    #[test]
    fn records_solid_fill() {
        let mut scene = TerminalScene::new();
        let color = Color::new([1.0, 0.0, 0.0, 1.0]);
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            color,
            None,
            &Rect::new(1.0, 2.0, 5.0, 6.0),
        );
        assert_eq!(scene.fills().len(), 1);
        assert_eq!(scene.fills()[0].x, 1.0);
        assert_eq!(scene.fills()[0].width, 4.0);
        assert_eq!(scene.fills()[0].color, color);
        assert!(scene.images().is_empty());
    }

    #[test]
    fn records_image_and_ignores_glyphs() {
        let image = ImageData {
            data: Blob::from(vec![255u8, 0, 0, 255, 0, 255, 0, 255]),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width: 2,
            height: 1,
        };
        let brush = peniko::ImageBrush {
            image: &image,
            sampler: ImageSampler::default(),
        };
        let mut scene = TerminalScene::new();
        scene.draw_image(brush, Affine::IDENTITY);
        assert!(scene.fills().is_empty());
        assert_eq!(scene.images().len(), 1);
        assert_eq!(scene.images()[0].image_width, 2);
        assert_eq!(scene.images()[0].pixels.len(), 8);
        assert_eq!(scene.images()[0].width, 2.0);

        scene.reset();
        assert!(scene.images().is_empty());
    }

    #[test]
    fn clip_layers_limit_fill_rectangles() {
        let mut scene = TerminalScene::new();
        scene.push_clip_layer(Affine::IDENTITY, &Rect::new(2.0, 2.0, 4.0, 4.0));
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::new([0.0, 1.0, 0.0, 1.0]),
            None,
            &Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        assert_eq!(scene.fills().len(), 1);
        assert_eq!(scene.fills()[0].x, 2.0);
        assert_eq!(scene.fills()[0].width, 2.0);
    }
}
