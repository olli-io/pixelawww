use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{
    AnimationDecoder, DynamicImage, Frames, GenericImageView, ImageError, ImageFormat,
    codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
};
use resvg::usvg::{Options, Tree};

use std::{
    io::{Cursor, Read, stdin},
    path::Path,
};

use common::{
    compression::{BitPack, Compressor},
    ipc::{self, Coord, Nanos, PixelFormat, Position},
};

use crate::cli::{CropGravity, ResizeStrategy};

use super::cli;

pub enum Format {
    Image(ImageFormat),
    JpegXL,
    Svg(Box<Tree>),
}

impl std::fmt::Debug for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image(arg0) => f.debug_tuple("Image").field(arg0).finish(),
            Self::JpegXL => f.debug_tuple("JpegXL").finish(),
            Self::Svg(_) => f.debug_tuple("Svg").finish(),
        }
    }
}

pub struct ImgBuf {
    bytes: Box<[u8]>,
    format: Format,
    is_animated: bool,
}

impl ImgBuf {
    /// Create a new ImgBuf from a given path. Use - for Stdin
    pub fn new(path: &Path) -> Result<Self, String> {
        let bytes = if let Some("-") = path.to_str() {
            let mut bytes = Vec::new();
            stdin()
                .read_to_end(&mut bytes)
                .map_err(|e| format!("failed to read standard input: {e}"))?;
            bytes
        } else {
            std::fs::read(path).map_err(|e| format!("failed to read file: {e}"))?
        };

        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|e| format!("failed to read image: {e}"))?;

        let format = reader.format();
        let is_animated = match format {
            Some(ImageFormat::Gif) => true,
            Some(ImageFormat::WebP) => WebPDecoder::new(Cursor::new(&bytes))
                .map_err(|e| format!("failed to decode Webp Image: {e}"))?
                .has_animation(),
            Some(ImageFormat::Png) => PngDecoder::new(Cursor::new(&bytes))
                .map_err(|e| format!("failed to decode Png Image: {e}"))?
                .is_apng()
                .map_err(|e| format!("failed to detect if Png is animated: {e}"))?,
            None => match reader.into_dimensions() {
                Ok(_) => {
                    return Ok(Self {
                        format: Format::JpegXL,
                        bytes: bytes.into_boxed_slice(),
                        is_animated: false,
                    });
                }
                Err(e) => match e {
                    ImageError::Unsupported(_) => {
                        match Tree::from_data(&bytes, &Options::default()) {
                            Ok(tree) => {
                                return Ok(Self {
                                    format: Format::Svg(Box::new(tree)),
                                    bytes: bytes.into_boxed_slice(),
                                    is_animated: false,
                                });
                            }
                            Err(e) => {
                                return Err(format!(
                                    "Unrecognized format by `image` crate. Also failed to decode as `svg`: {e}."
                                ));
                            }
                        }
                    }
                    _ => {
                        return Err(format!(
                            "Format recognized by `image` crate but another error occurred: {e}."
                        ));
                    }
                },
            },
            _ => false,
        };

        Ok(Self {
            format: Format::Image(format.unwrap()), // this is ok because we return err earlier if it is None
            bytes: bytes.into_boxed_slice(),
            is_animated,
        })
    }

    pub fn is_animated(&self) -> bool {
        self.is_animated
    }

    /// Decode the ImgBuf into an RgbImage
    pub fn decode_prepare(&'_ self) -> DecodeBuffer<'_> {
        match &self.format {
            Format::Image(_) | Format::JpegXL => DecodeBuffer::RasterImage(RasterImage(self)),
            Format::Svg(tree) => DecodeBuffer::VectorImage(VectorImage(tree)),
        }
    }

    /// Convert this ImgBuf into Frames
    pub fn as_frames(&'_ self) -> Result<Frames<'_>, String> {
        match self.format {
            Format::Image(ImageFormat::Gif) => Ok(GifDecoder::new(Cursor::new(&self.bytes))
                .map_err(|e| format!("failed to decode gif during animation: {e}"))?
                .into_frames()),
            Format::Image(ImageFormat::WebP) => Ok(WebPDecoder::new(Cursor::new(&self.bytes))
                .map_err(|e| format!("failed to decode webp during animation: {e}"))?
                .into_frames()),
            Format::Image(ImageFormat::Png) => Ok(PngDecoder::new(Cursor::new(&self.bytes))
                .map_err(|e| format!("failed to decode png during animation: {e}"))?
                .apng()
                .unwrap() // we detected this earlier
                .into_frames()),
            _ => Err(format!(
                "requested format has no decoder: {:#?}",
                self.format
            )),
        }
    }
}

pub struct RasterImage<'a>(&'a ImgBuf);
pub struct VectorImage<'a>(&'a Tree);

impl RasterImage<'_> {
    pub fn decode(&self, format: PixelFormat) -> Result<Image, String> {
        let imgbuf = self.0;
        let reader = image::ImageReader::new(Cursor::new(&imgbuf.bytes))
            .with_guessed_format()
            .map_err(|e| format!("failed to read image: {e}"))?;
        let dynimage = reader
            .decode()
            .map_err(|e| format!("failed to decode image: {e}"))?;

        let width = dynimage.width();
        let height = dynimage.height();

        let bytes = {
            let mut img = if format.channels() == 3 {
                dynimage.into_rgb8().into_raw().into_boxed_slice()
            } else {
                dynimage.into_rgba8().into_raw().into_boxed_slice()
            };

            if format.must_swap_r_and_b_channels() {
                for pixel in img.chunks_exact_mut(format.channels() as usize) {
                    pixel.swap(0, 2);
                }
            }
            img
        };

        Ok(Image {
            width,
            height,
            format,
            bytes,
        })
    }

    pub fn is_animated(&self) -> bool {
        self.0.is_animated()
    }

    pub fn as_frames(&self) -> Result<Frames<'_>, String> {
        self.0.as_frames()
    }
}

impl VectorImage<'_> {
    pub fn decode(&self, format: PixelFormat, width: u32, height: u32) -> Result<Image, String> {
        use resvg::{tiny_skia::PixmapMut, usvg::Transform};
        let tree = self.0;
        let scale = {
            let size = tree.size();
            let ratio = size.width() / size.height();
            let w = width as f32;
            let h = height as f32;
            let img_r = w / h;
            if ratio < img_r {
                h / size.height()
            } else {
                w / size.width()
            }
        };
        let transform = Transform::from_scale(scale, scale);

        let (width, height) = (
            (tree.size().width() * scale) as u32,
            (tree.size().height() * scale) as u32,
        );
        let mut bytes = vec![0; (width * height * 4) as usize];
        let mut pixmap = match PixmapMut::from_bytes(&mut bytes, width, height) {
            Some(pixmap) => pixmap,
            None => return Err("failed to create pixmap to render svg".to_string()),
        };
        resvg::render(tree, transform, &mut pixmap);
        let dynimage =
            DynamicImage::ImageRgba8(image::RgbaImage::from_raw(width, height, bytes).unwrap());

        let bytes = {
            let mut img = if format.channels() == 3 {
                dynimage.into_rgb8().into_raw().into_boxed_slice()
            } else {
                dynimage.into_rgba8().into_raw().into_boxed_slice()
            };

            if format.must_swap_r_and_b_channels() {
                for pixel in img.chunks_exact_mut(format.channels() as usize) {
                    pixel.swap(0, 2);
                }
            }
            img
        };

        Ok(Image {
            width,
            height,
            format,
            bytes,
        })
    }
}

pub enum DecodeBuffer<'a> {
    RasterImage(RasterImage<'a>),
    VectorImage(VectorImage<'a>),
}

/// Created by decoding a RasterImage or a VectorImage
pub struct Image {
    width: u32,
    height: u32,
    format: PixelFormat,
    bytes: Box<[u8]>,
}

impl Image {
    #[must_use]
    fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> Self {
        // make sure we don't crop a region larger than the image
        let x = x.min(self.width) as usize;
        let y = y.min(self.height) as usize;
        let width = (width as usize).min(self.width as usize - x);
        let height = (height as usize).min(self.height as usize - y);

        let mut bytes = Vec::with_capacity(width * height * self.format.channels() as usize);

        let begin = ((y * self.width as usize) + x) * self.format.channels() as usize;
        let stride = self.width as usize * self.format.channels() as usize;
        let row_size = width * self.format.channels() as usize;

        for row_index in 0..height {
            let row = begin + row_index * stride;
            bytes.extend_from_slice(&self.bytes[row..row + row_size]);
        }

        Self {
            width: width as u32,
            height: height as u32,
            bytes: bytes.into_boxed_slice(),
            format: self.format,
        }
    }

    fn from_frame(frame: image::Frame, format: PixelFormat) -> Self {
        let dynimage = DynamicImage::ImageRgba8(frame.into_buffer());
        let (width, height) = dynimage.dimensions();

        // NOTE: when animating frames, we ALWAYS use 3 channels

        let format = match format {
            PixelFormat::Bgr | PixelFormat::Abgr => PixelFormat::Bgr,
            PixelFormat::Rgb | PixelFormat::Argb => PixelFormat::Rgb,
        };

        let mut bytes = dynimage.into_rgb8().into_raw().into_boxed_slice();
        if format.must_swap_r_and_b_channels() {
            for pixel in bytes.chunks_exact_mut(3) {
                pixel.swap(0, 2);
            }
        }

        Self {
            width,
            height,
            format,
            bytes,
        }
    }
}

pub fn compress_frames(
    mut frames: Frames,
    dim: (u32, u32),
    format: PixelFormat,
    filter: FilterType,
    resize: ResizeStrategy,
    color: [u8; 4],
) -> Result<Vec<(BitPack, Nanos)>, String> {
    let mut compressor = Compressor::new();
    let mut compressed_frames = Vec::new();

    // The first frame should always exist
    let first = frames.next().unwrap().unwrap();
    let first_duration = first.delay().numer_denom_ms();
    let mut first_duration = Nanos::from_millis((first_duration.0 / first_duration.1).into());
    let first_img = Image::from_frame(first, format);
    let first_img = match resize {
        ResizeStrategy::No => img_pad(&first_img, dim, color),
        ResizeStrategy::Crop => img_resize_crop(&first_img, dim, filter, None)?,
        ResizeStrategy::Fit => img_resize_fit(&first_img, dim, filter, color)?,
        ResizeStrategy::Stretch => img_resize_stretch(&first_img, dim, filter)?,
    };

    let mut canvas: Option<Box<[u8]>> = None;
    while let Some(Ok(frame)) = frames.next() {
        let (dur_num, dur_div) = frame.delay().numer_denom_ms();
        let duration = Nanos::from_millis((dur_num / dur_div).into());

        let img = Image::from_frame(frame, format);
        let img = match resize {
            ResizeStrategy::No => img_pad(&img, dim, color),
            ResizeStrategy::Crop => img_resize_crop(&img, dim, filter, None)?,
            ResizeStrategy::Fit => img_resize_fit(&img, dim, filter, color)?,
            ResizeStrategy::Stretch => img_resize_stretch(&img, dim, filter)?,
        };

        if let Some(canvas) = canvas.as_ref() {
            match compressor.compress(canvas, &img, format) {
                Some(bytes) => compressed_frames.push((bytes, duration)),
                None => match compressed_frames.last_mut() {
                    Some(last) => last.1 += duration,
                    None => first_duration += duration,
                },
            }
        } else {
            match compressor.compress(&first_img, &img, format) {
                Some(bytes) => compressed_frames.push((bytes, duration)),
                None => first_duration += duration,
            }
        }
        canvas = Some(img);
    }

    //Add the first frame we got earlier:
    if let Some(canvas) = canvas.as_ref() {
        match compressor.compress(canvas, &first_img, format) {
            Some(bytes) => compressed_frames.push((bytes, first_duration)),
            None => match compressed_frames.last_mut() {
                Some(last) => last.1 += first_duration,
                None => first_duration += first_duration,
            },
        }
    }

    Ok(compressed_frames)
}

pub fn make_filter(filter: cli::Filter) -> fast_image_resize::FilterType {
    match filter {
        cli::Filter::Nearest => fast_image_resize::FilterType::Box,
        cli::Filter::Bilinear => fast_image_resize::FilterType::Bilinear,
        cli::Filter::CatmullRom => fast_image_resize::FilterType::CatmullRom,
        cli::Filter::Mitchell => fast_image_resize::FilterType::Mitchell,
        cli::Filter::Lanczos3 => fast_image_resize::FilterType::Lanczos3,
    }
}

pub fn img_pad(img: &Image, dimensions: (u32, u32), color: [u8; 4]) -> Box<[u8]> {
    let channels = img.format.channels() as usize;

    let mut color4 = color.to_owned();
    let mut color3 = [color[0], color[1], color[2]];
    let color: &mut [u8] = if channels == 3 {
        &mut color3
    } else {
        &mut color4
    };

    if img.format.must_swap_r_and_b_channels() {
        color.swap(0, 2);
    }
    let (padded_w, padded_h) = dimensions;
    let (padded_w, padded_h) = (padded_w as usize, padded_h as usize);
    let mut padded = Vec::with_capacity(padded_h * padded_w * channels);

    let img = if img.width > dimensions.0 || img.height > dimensions.1 {
        let left = img.width.saturating_sub(dimensions.0) / 2;
        let top = img.height.saturating_sub(dimensions.1) / 2;
        img.crop(left, top, dimensions.0, dimensions.1)
    } else {
        img.crop(0, 0, dimensions.0, dimensions.1)
    };

    let (img_w, img_h) = (
        (img.width as usize).min(padded_w),
        (img.height as usize).min(padded_h),
    );

    for _ in 0..(((padded_h - img_h) / 2) * padded_w) {
        padded.extend_from_slice(color);
    }

    // Calculate left and right border widths. `usize::div` rounds toward 0, so, if
    // `padded_w - img_w` is odd, add an extra pixel to the right border to ensure the row is the
    // correct width.
    let left_border_w = (padded_w - img_w) / 2;
    let right_border_w = left_border_w + ((padded_w - img_w) % 2);

    for row in 0..img_h {
        for _ in 0..left_border_w {
            padded.extend_from_slice(color);
        }

        padded.extend_from_slice(
            &img.bytes[(row * img_w * channels)..((row + 1) * img_w * channels)],
        );

        for _ in 0..right_border_w {
            padded.extend_from_slice(color);
        }
    }

    while padded.len() < (padded_h * padded_w * channels) {
        padded.extend_from_slice(color);
    }

    padded.into_boxed_slice()
}

/// Resize an image to fit within the given dimensions, covering as much space as possible without
/// cropping.
pub fn img_resize_fit(
    img: &Image,
    dimensions: (u32, u32),
    filter: FilterType,
    padding_color: [u8; 4],
) -> Result<Box<[u8]>, String> {
    let (width, height) = dimensions;
    if (img.width, img.height) == (width, height) {
        Ok(img.bytes.clone())
    } else {
        // if our image is already scaled to fit, skip resizing it and just pad it directly
        if img.width == width || img.height == height {
            return Ok(img_pad(img, dimensions, padding_color));
        }

        let ratio = width as f32 / height as f32;
        let img_r = img.width as f32 / img.height as f32;

        let (trg_w, trg_h) = if ratio > img_r {
            let scale = height as f32 / img.height as f32;
            ((img.width as f32 * scale) as u32, height)
        } else {
            let scale = width as f32 / img.width as f32;
            (width, (img.height as f32 * scale) as u32)
        };

        let pixel_type = if img.format.channels() == 3 {
            PixelType::U8x3
        } else {
            PixelType::U8x4
        };
        let src = match fast_image_resize::images::ImageRef::new(
            img.width,
            img.height,
            img.bytes.as_ref(),
            pixel_type,
        ) {
            Ok(i) => i,
            Err(e) => return Err(e.to_string()),
        };

        let mut dst = fast_image_resize::images::Image::new(trg_w, trg_h, pixel_type);
        let mut resizer = Resizer::new();
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter));

        if let Err(e) = resizer.resize(&src, &mut dst, Some(&options)) {
            return Err(e.to_string());
        }

        let img = Image {
            width: trg_w,
            height: trg_h,
            format: img.format,
            bytes: dst.into_vec().into_boxed_slice(),
        };
        Ok(img_pad(&img, dimensions, padding_color))
    }
}

pub fn img_resize_stretch(
    img: &Image,
    dimensions: (u32, u32),
    filter: FilterType,
) -> Result<Box<[u8]>, String> {
    let (width, height) = dimensions;
    let resized_img = if (img.width, img.height) == (width, height) {
        img.bytes.clone()
    } else {
        let pixel_type = if img.format.channels() == 3 {
            PixelType::U8x3
        } else {
            PixelType::U8x4
        };

        let src = match fast_image_resize::images::ImageRef::new(
            img.width,
            img.height,
            img.bytes.as_ref(),
            pixel_type,
        ) {
            Ok(i) => i,
            Err(e) => return Err(e.to_string()),
        };

        let mut dst = fast_image_resize::images::Image::new(width, height, pixel_type);
        let mut resizer = Resizer::new();
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter));

        if let Err(e) = resizer.resize(&src, &mut dst, Some(&options)) {
            return Err(e.to_string());
        }

        dst.into_vec().into_boxed_slice()
    };

    Ok(resized_img)
}

pub fn img_resize_crop(
    img: &Image,
    dimensions: (u32, u32),
    filter: FilterType,
    crop_gravity: Option<CropGravity>,
) -> Result<Box<[u8]>, String> {
    let (width, height) = dimensions;
    let resized_img = if (img.width, img.height) == (width, height) {
        img.bytes.clone()
    } else {
        let pixel_type = if img.format.channels() == 3 {
            PixelType::U8x3
        } else {
            PixelType::U8x4
        };
        let src = match fast_image_resize::images::ImageRef::new(
            img.width,
            img.height,
            img.bytes.as_ref(),
            pixel_type,
        ) {
            Ok(i) => i,
            Err(e) => return Err(e.to_string()),
        };

        let centering_tuple = crop_gravity.unwrap_or_default().as_centering_tuple();

        let mut dst = fast_image_resize::images::Image::new(width, height, pixel_type);
        let mut resizer = Resizer::new();
        let options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(filter))
            .fit_into_destination(Some(centering_tuple));

        if let Err(e) = resizer.resize(&src, &mut dst, Some(&options)) {
            return Err(e.to_string());
        }

        dst.into_vec().into_boxed_slice()
    };

    Ok(resized_img)
}

pub fn make_transition(img: &cli::Img) -> ipc::Transition {
    let mut angle = img.transition_angle;
    let mut wave = img.transition_wave;
    let step = img.transition_step;

    let x = match img.transition_pos.x {
        cli::CliCoord::Percent(x) => {
            if !(0.0..=1.0).contains(&x) {
                println!(
                    "Warning: x value not in range [0,1] position might be set outside screen: {x}"
                );
            }
            Coord::Percent(x)
        }
        cli::CliCoord::Pixel(x) => Coord::Pixel(x),
    };

    let y = match img.transition_pos.y {
        cli::CliCoord::Percent(y) => {
            if !(0.0..=1.0).contains(&y) {
                println!(
                    "Warning: y value not in range [0,1] position might be set outside screen: {y}"
                );
            }
            Coord::Percent(y)
        }
        cli::CliCoord::Pixel(y) => Coord::Pixel(y),
    };

    let mut pos = Position::new(x, y);

    let transition_type = match img.transition_type {
        cli::TransitionType::None => ipc::TransitionType::None,
        cli::TransitionType::Simple => ipc::TransitionType::Simple,
        cli::TransitionType::Fade => ipc::TransitionType::Fade,
        cli::TransitionType::Wipe => {
            wave = (0.0, 0.0);
            ipc::TransitionType::Wipe
        }
        cli::TransitionType::Outer => ipc::TransitionType::Outer,
        cli::TransitionType::Grow => ipc::TransitionType::Grow,
        cli::TransitionType::Wave => ipc::TransitionType::Wave,
        cli::TransitionType::Right => {
            angle = 0.0;
            ipc::TransitionType::Wipe
        }
        cli::TransitionType::Top => {
            angle = 90.0;
            ipc::TransitionType::Wipe
        }
        cli::TransitionType::Left => {
            angle = 180.0;
            ipc::TransitionType::Wipe
        }
        cli::TransitionType::Bottom => {
            angle = 270.0;
            ipc::TransitionType::Wipe
        }
        cli::TransitionType::Center => {
            pos = Position::new(Coord::Percent(0.5), Coord::Percent(0.5));
            ipc::TransitionType::Grow
        }
        cli::TransitionType::Any => {
            pos = Position::new(
                Coord::Percent(fastrand::f32()),
                Coord::Percent(fastrand::f32()),
            );
            if fastrand::bool() {
                ipc::TransitionType::Grow
            } else {
                ipc::TransitionType::Outer
            }
        }
        cli::TransitionType::Random => {
            pos = Position::new(
                Coord::Percent(fastrand::f32()),
                Coord::Percent(fastrand::f32()),
            );
            angle = fastrand::f64() * 360.0;
            wave = (fastrand::f32() * 100.0, fastrand::f32() * 100.0);

            match fastrand::u8(0..4) {
                0 => ipc::TransitionType::Fade,
                1 => ipc::TransitionType::Wave,
                2 => ipc::TransitionType::Outer,
                3 => ipc::TransitionType::Grow,
                _ => unreachable!(),
            }
        }
    };

    let transition = ipc::Transition {
        duration: img.transition_duration,
        step,
        fps: img.transition_fps,
        bezier: img.transition_bezier,
        angle,
        pos,
        transition_type,
        wave,
        invert_y: img.invert_y,
    };

    common::log::debug!("transition: {transition:#?}");
    transition
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_image(width: u32, height: u32, format: PixelFormat, pixel: [u8; 4]) -> Image {
        let channels = format.channels() as usize;
        let mut bytes = Vec::with_capacity(width as usize * height as usize * channels);
        for _ in 0..(width * height) {
            bytes.extend_from_slice(&pixel[..channels]);
        }
        Image {
            width,
            height,
            format,
            bytes: bytes.into_boxed_slice(),
        }
    }

    #[test]
    fn img_pad_preserves_row_stride_across_parities() {
        let fg = [10, 20, 30, 255];
        let bg = [1, 2, 3, 255];

        for padded_w in [10u32, 11] {
            for padded_h in [8u32, 9] {
                for img_w in [4u32, 5, padded_w] {
                    for img_h in [4u32, 5, padded_h] {
                        let img = solid_image(img_w, img_h, PixelFormat::Argb, fg);
                        let out = img_pad(&img, (padded_w, padded_h), bg);
                        assert_eq!(out.len(), (padded_w * padded_h * 4) as usize);

                        let top_pad = ((padded_h - img_h) / 2) as usize;
                        let left_pad = ((padded_w - img_w) / 2) as usize;
                        let right_pad = (padded_w as usize) - (img_w as usize) - left_pad;

                        // Check a row inside the image region is not skewed
                        let row_idx = top_pad;
                        let row_start = row_idx * (padded_w as usize) * 4;
                        let row = &out[row_start..row_start + (padded_w as usize) * 4];

                        // Note: PixelFormat::Argb swaps R and B on `bg` inside `img_pad`
                        let channels = img.format.channels() as usize;
                        let expected_bg = [bg[2], bg[1], bg[0], bg[3]];
                        for px in row[..left_pad * channels].chunks_exact(channels) {
                            assert_eq!(px, expected_bg);
                        }
                        for px in row[left_pad * channels..(left_pad + img_w as usize) * channels]
                            .chunks_exact(channels)
                        {
                            assert_eq!(px, fg);
                        }
                        for px in
                            row[(padded_w as usize - right_pad) * channels..].chunks_exact(channels)
                        {
                            assert_eq!(px, expected_bg);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn img_pad_mixed_oversized_dimensions_does_not_underflow() {
        let fg = [10, 20, 30, 255];
        let bg = [0, 0, 0, 255];

        let wide_short = solid_image(20, 4, PixelFormat::Argb, fg);
        let out = img_pad(&wide_short, (11, 9), bg);
        assert_eq!(out.len(), 11 * 9 * 4);

        let narrow_tall = solid_image(4, 20, PixelFormat::Argb, fg);
        let out = img_pad(&narrow_tall, (11, 9), bg);
        assert_eq!(out.len(), 11 * 9 * 4);
    }

    #[test]
    fn img_resize_fit_odd_target_dimensions() {
        let fg = [100, 150, 200, 255];
        let bg = [0, 0, 0, 255];

        // 1500x681 and 540x810 onto 1919x1080 (from issue #527)
        for (src_w, src_h) in [(150, 68), (54, 81)] {
            let img = solid_image(src_w, src_h, PixelFormat::Argb, fg);
            let out = img_resize_fit(&img, (191, 108), FilterType::Box, bg).unwrap();
            assert_eq!(out.len(), 191 * 108 * 4);
        }
    }
}
