use dyn_any::{DynAny, StaticType};
use glam::{DAffine2, DVec2};
use graph_craft::imaginate_input::{ImaginateController, ImaginateMaskStartingFill, ImaginateSamplingMethod};
use graph_craft::proto::DynFuture;
use graphene_core::raster::{Alpha, BlendMode, BlendNode, Image, ImageFrame, Linear, LinearChannel, Luminance, Pixel, RGBMut, Raster, RasterMut, RedGreenBlue, Sample};
use graphene_core::transform::Transform;

use crate::wasm_application_io::WasmEditorApi;
use graphene_core::raster::bbox::{AxisAlignedBbox, Bbox};
use graphene_core::value::CopiedNode;
use graphene_core::{Color, Node};

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::path::Path;

#[derive(Debug, DynAny)]
pub enum Error {
	IO(std::io::Error),
	Image(image::ImageError),
}

impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		Error::IO(e)
	}
}

pub trait FileSystem {
	fn open<P: AsRef<Path>>(&self, path: P) -> Result<Box<dyn std::io::Read>, Error>;
}

#[derive(Clone)]
pub struct StdFs;
impl FileSystem for StdFs {
	fn open<P: AsRef<Path>>(&self, path: P) -> Result<Reader, Error> {
		Ok(Box::new(std::fs::File::open(path)?))
	}
}
type Reader = Box<dyn std::io::Read>;

pub struct FileNode<FileSystem> {
	fs: FileSystem,
}
#[node_macro::node_fn(FileNode)]
fn file_node<P: AsRef<Path>, FS: FileSystem>(path: P, fs: FS) -> Result<Reader, Error> {
	fs.open(path)
}

pub struct BufferNode;
#[node_macro::node_fn(BufferNode)]
fn buffer_node<R: std::io::Read>(reader: R) -> Result<Vec<u8>, Error> {
	Ok(std::io::Read::bytes(reader).collect::<Result<Vec<_>, _>>()?)
}

pub struct DownresNode<P> {
	_p: PhantomData<P>,
}

#[node_macro::node_fn(DownresNode<_P>)]
fn downres<_P: Pixel>(image_frame: ImageFrame<_P>) -> ImageFrame<_P> {
	let target_width = (image_frame.transform.transform_vector2((1., 0.).into()).length() as usize).min(image_frame.image.width as usize);
	let target_height = (image_frame.transform.transform_vector2((0., 1.).into()).length() as usize).min(image_frame.image.height as usize);

	let mut image = Image {
		width: target_width as u32,
		height: target_height as u32,
		data: Vec::with_capacity(target_width * target_height),
	};

	let scale_factor = DVec2::new(image_frame.image.width as f64, image_frame.image.height as f64) / DVec2::new(target_width as f64, target_height as f64);
	for y in 0..target_height {
		for x in 0..target_width {
			let pixel = image_frame.sample(DVec2::new(x as f64, y as f64) * scale_factor);
			image.data.push(pixel);
		}
	}

	ImageFrame {
		image,
		transform: image_frame.transform,
	}
}

#[derive(Debug, Clone, Copy)]
pub struct MapImageNode<P, MapFn> {
	map_fn: MapFn,
	_p: PhantomData<P>,
}

#[node_macro::node_fn(MapImageNode<_P>)]
fn map_image<MapFn, _P, Img: RasterMut<Pixel = _P>>(image: Img, map_fn: &'input MapFn) -> Img
where
	MapFn: for<'any_input> Node<'any_input, _P, Output = _P> + 'input,
{
	let mut image = image;

	image.map_pixels(|c| map_fn.eval(c));
	image
}

#[derive(Debug, Clone, Copy)]
pub struct InsertChannelNode<P, S, Insertion, TargetChannel> {
	insertion: Insertion,
	target_channel: TargetChannel,
	_p: PhantomData<P>,
	_s: PhantomData<S>,
}

#[node_macro::node_fn(InsertChannelNode<_P, _S>)]
fn insert_channel_node<
	// _P is the color of the input image.
	_P: RGBMut,
	_S: Pixel + Luminance,
	// Input image
	Input: RasterMut<Pixel = _P>,
	Insertion: Raster<Pixel = _S>,
>(
	mut image: Input,
	insertion: Insertion,
	target_channel: RedGreenBlue,
) -> Input
where
	_P::ColorChannel: Linear,
{
	if insertion.width() == 0 {
		return image;
	}

	if insertion.width() != image.width() || insertion.height() != image.height() {
		log::warn!("Stencil and image have different sizes. This is not supported.");
		return image;
	}

	for y in 0..image.height() {
		for x in 0..image.width() {
			let image_pixel = image.get_pixel_mut(x, y).unwrap();
			let insertion_pixel = insertion.get_pixel(x, y).unwrap();
			match target_channel {
				RedGreenBlue::Red => image_pixel.set_red(insertion_pixel.l().cast_linear_channel()),
				RedGreenBlue::Green => image_pixel.set_green(insertion_pixel.l().cast_linear_channel()),
				RedGreenBlue::Blue => image_pixel.set_blue(insertion_pixel.l().cast_linear_channel()),
			}
		}
	}

	image
}

#[derive(Debug, Clone, Copy)]
pub struct MaskImageNode<P, S, Stencil> {
	stencil: Stencil,
	_p: PhantomData<P>,
	_s: PhantomData<S>,
}

#[node_macro::node_fn(MaskImageNode<_P, _S>)]
fn mask_imge<
	// _P is the color of the input image. It must have an alpha channel because that is going to
	// be modified by the mask
	_P: Copy + Alpha,
	// _S is the color of the stencil. It must have a luminance channel because that is used to
	// mask the input image
	_S: Luminance,
	// Input image
	Input: Transform + RasterMut<Pixel = _P>,
	// Stencil
	Stencil: Transform + Sample<Pixel = _S>,
>(
	mut image: Input,
	stencil: Stencil,
) -> Input {
	let image_size = DVec2::new(image.width() as f64, image.height() as f64);
	let mask_size = stencil.transform().decompose_scale();

	if mask_size == DVec2::ZERO {
		return image;
	}

	// Transforms a point from the background image to the forground image
	let bg_to_fg = image.transform() * DAffine2::from_scale(1. / image_size);
	let stencil_transform_inverse = stencil.transform().inverse();

	let area = bg_to_fg.transform_vector2(DVec2::ONE);
	for y in 0..image.height() {
		for x in 0..image.width() {
			let image_point = DVec2::new(x as f64, y as f64);
			let mut mask_point = bg_to_fg.transform_point2(image_point);
			let local_mask_point = stencil_transform_inverse.transform_point2(mask_point);
			mask_point = stencil.transform().transform_point2(local_mask_point.clamp(DVec2::ZERO, DVec2::ONE));

			let image_pixel = image.get_pixel_mut(x, y).unwrap();
			if let Some(mask_pixel) = stencil.sample(mask_point, area) {
				*image_pixel = image_pixel.multiplied_alpha(mask_pixel.l().cast_linear_channel());
			}
		}
	}

	image
}

#[derive(Debug, Clone, Copy)]
pub struct BlendImageTupleNode<P, Fg, MapFn> {
	map_fn: MapFn,
	_p: PhantomData<P>,
	_fg: PhantomData<Fg>,
}

#[node_macro::node_fn(BlendImageTupleNode<_P, _Fg>)]
fn blend_image_tuple<_P: Alpha + Pixel + Debug, MapFn, _Fg: Sample<Pixel = _P> + Transform>(images: (ImageFrame<_P>, _Fg), map_fn: &'input MapFn) -> ImageFrame<_P>
where
	MapFn: for<'any_input> Node<'any_input, (_P, _P), Output = _P> + 'input + Clone,
{
	let (background, foreground) = images;

	blend_image(foreground, background, map_fn)
}

#[derive(Debug, Clone, Copy)]
pub struct BlendImageNode<P, Background, MapFn> {
	background: Background,
	map_fn: MapFn,
	_p: PhantomData<P>,
}

#[node_macro::node_fn(BlendImageNode<_P>)]
async fn blend_image_node<_P: Alpha + Pixel + Debug, Forground: Sample<Pixel = _P> + Transform>(
	foreground: Forground,
	background: ImageFrame<_P>,
	map_fn: impl Node<(_P, _P), Output = _P>,
) -> ImageFrame<_P> {
	blend_new_image(foreground, background, &self.map_fn)
}

#[derive(Debug, Clone, Copy)]
pub struct BlendReverseImageNode<P, Background, MapFn> {
	background: Background,
	map_fn: MapFn,
	_p: PhantomData<P>,
}

#[node_macro::node_fn(BlendReverseImageNode<_P>)]
fn blend_image_node<_P: Alpha + Pixel + Debug, MapFn, Background: Transform + Sample<Pixel = _P>>(foreground: ImageFrame<_P>, background: Background, map_fn: &'input MapFn) -> ImageFrame<_P>
where
	MapFn: for<'any_input> Node<'any_input, (_P, _P), Output = _P> + 'input,
{
	blend_new_image(background, foreground, map_fn)
}

fn blend_new_image<'input, _P: Alpha + Pixel + Debug, MapFn, Frame: Sample<Pixel = _P> + Transform>(foreground: Frame, background: ImageFrame<_P>, map_fn: &'input MapFn) -> ImageFrame<_P>
where
	MapFn: Node<'input, (_P, _P), Output = _P>,
{
	let foreground_aabb = Bbox::unit().affine_transform(foreground.transform()).to_axis_aligned_bbox();
	let background_aabb = Bbox::unit().affine_transform(background.transform()).to_axis_aligned_bbox();

	let Some(aabb) = foreground_aabb.union_non_empty(&background_aabb) else {
		return ImageFrame::empty();
	};

	if background_aabb.contains(foreground_aabb.start) && background_aabb.contains(foreground_aabb.end) {
		return blend_image(foreground, background, map_fn);
	}

	// Clamp the foreground image to the background image
	let start = aabb.start.as_uvec2();
	let end = aabb.end.as_uvec2();

	let new_background = Image::new(end.x - start.x, end.y - start.y, _P::TRANSPARENT);
	let size = DVec2::new(new_background.width as f64, new_background.height as f64);
	let transfrom = DAffine2::from_scale_angle_translation(size, 0., start.as_dvec2());
	let mut new_background = ImageFrame {
		image: new_background,
		transform: transfrom,
	};

	new_background = blend_image(background, new_background, map_fn);
	blend_image(foreground, new_background, map_fn)
}

fn blend_image<'input, _P: Alpha + Pixel + Debug, MapFn, Frame: Sample<Pixel = _P> + Transform, Background: RasterMut<Pixel = _P> + Transform + Sample<Pixel = _P>>(
	foreground: Frame,
	background: Background,
	map_fn: &'input MapFn,
) -> Background
where
	MapFn: Node<'input, (_P, _P), Output = _P>,
{
	blend_image_closure(foreground, background, |a, b| map_fn.eval((a, b)))
}

pub fn blend_image_closure<_P: Alpha + Pixel + Debug, MapFn, Frame: Sample<Pixel = _P> + Transform, Background: RasterMut<Pixel = _P> + Transform + Sample<Pixel = _P>>(
	foreground: Frame,
	mut background: Background,
	map_fn: MapFn,
) -> Background
where
	MapFn: Fn(_P, _P) -> _P,
{
	let background_size = DVec2::new(background.width() as f64, background.height() as f64);
	// Transforms a point from the background image to the forground image
	let bg_to_fg = background.transform() * DAffine2::from_scale(1. / background_size);

	// Footprint of the foreground image (0,0) (1, 1) in the background image space
	let bg_aabb = Bbox::unit().affine_transform(background.transform().inverse() * foreground.transform()).to_axis_aligned_bbox();

	// Clamp the foreground image to the background image
	let start = (bg_aabb.start * background_size).max(DVec2::ZERO).as_uvec2();
	let end = (bg_aabb.end * background_size).min(background_size).as_uvec2();

	let area = bg_to_fg.transform_point2(DVec2::new(1., 1.)) - bg_to_fg.transform_point2(DVec2::ZERO);
	for y in start.y..end.y {
		for x in start.x..end.x {
			let bg_point = DVec2::new(x as f64, y as f64);
			let fg_point = bg_to_fg.transform_point2(bg_point);

			if let Some(src_pixel) = foreground.sample(fg_point, area) {
				if let Some(dst_pixel) = background.get_pixel_mut(x, y) {
					*dst_pixel = map_fn(src_pixel, *dst_pixel);
				}
			}
		}
	}

	background
}

#[derive(Debug, Clone, Copy)]
pub struct ExtendImageNode<Background> {
	background: Background,
}

#[node_macro::node_fn(ExtendImageNode)]
fn extend_image_node(foreground: ImageFrame<Color>, background: ImageFrame<Color>) -> ImageFrame<Color> {
	let foreground_aabb = Bbox::unit().affine_transform(foreground.transform()).to_axis_aligned_bbox();
	let background_aabb = Bbox::unit().affine_transform(background.transform()).to_axis_aligned_bbox();

	if foreground_aabb.contains(background_aabb.start) && foreground_aabb.contains(background_aabb.end) {
		return foreground;
	}

	blend_image(foreground, background, &BlendNode::new(CopiedNode::new(BlendMode::Normal), CopiedNode::new(100.)))
}

#[derive(Debug, Clone, Copy)]
pub struct ExtendImageToBoundsNode<Bounds> {
	bounds: Bounds,
}

#[node_macro::node_fn(ExtendImageToBoundsNode)]
fn extend_image_to_bounds_node(image: ImageFrame<Color>, bounds: DAffine2) -> ImageFrame<Color> {
	let image_aabb = Bbox::unit().affine_transform(image.transform()).to_axis_aligned_bbox();
	let bounds_aabb = Bbox::unit().affine_transform(bounds.transform()).to_axis_aligned_bbox();
	if image_aabb.contains(bounds_aabb.start) && image_aabb.contains(bounds_aabb.end) {
		return image;
	}

	if image.image.width == 0 || image.image.height == 0 {
		return EmptyImageNode::new(CopiedNode::new(Color::TRANSPARENT)).eval(bounds);
	}

	let orig_image_scale = DVec2::new(image.image.width as f64, image.image.height as f64);
	let layer_to_image_space = DAffine2::from_scale(orig_image_scale) * image.transform.inverse();
	let bounds_in_image_space = Bbox::unit().affine_transform(layer_to_image_space * bounds).to_axis_aligned_bbox();

	let new_start = bounds_in_image_space.start.floor().min(DVec2::ZERO);
	let new_end = bounds_in_image_space.end.ceil().max(orig_image_scale);
	let new_scale = new_end - new_start;

	// Copy over original image into embiggened image.
	let mut new_img = Image::new(new_scale.x as u32, new_scale.y as u32, Color::TRANSPARENT);
	let offset_in_new_image = (-new_start).as_uvec2();
	for y in 0..image.image.height {
		let old_start = y * image.image.width;
		let new_start = (y + offset_in_new_image.y) * new_img.width + offset_in_new_image.x;
		let old_row = &image.image.data[old_start as usize..(old_start + image.image.width) as usize];
		let new_row = &mut new_img.data[new_start as usize..(new_start + image.image.width) as usize];
		new_row.copy_from_slice(old_row);
	}

	// Compute new transform.
	// let layer_to_new_texture_space = (DAffine2::from_scale(1. / new_scale) * DAffine2::from_translation(new_start) * layer_to_image_space).inverse();
	let new_texture_to_layer_space = image.transform * DAffine2::from_scale(1.0 / orig_image_scale) * DAffine2::from_translation(new_start) * DAffine2::from_scale(new_scale);
	ImageFrame {
		image: new_img,
		transform: new_texture_to_layer_space,
	}
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergeBoundingBoxNode<Data> {
	_data: PhantomData<Data>,
}

#[node_macro::node_fn(MergeBoundingBoxNode<_Data>)]
fn merge_bounding_box_node<_Data: Transform>(input: (Option<AxisAlignedBbox>, _Data)) -> Option<AxisAlignedBbox> {
	let (initial_aabb, data) = input;

	let snd_aabb = Bbox::unit().affine_transform(data.transform()).to_axis_aligned_bbox();

	if let Some(fst_aabb) = initial_aabb {
		fst_aabb.union_non_empty(&snd_aabb)
	} else {
		Some(snd_aabb)
	}
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmptyImageNode<P, FillColor> {
	pub color: FillColor,
	_p: PhantomData<P>,
}

#[node_macro::node_fn(EmptyImageNode<_P>)]
fn empty_image<_P: Pixel>(transform: DAffine2, color: _P) -> ImageFrame<_P> {
	let width = transform.transform_vector2(DVec2::new(1., 0.)).length() as u32;
	let height = transform.transform_vector2(DVec2::new(0., 1.)).length() as u32;

	let image = Image::new(width, height, color);
	ImageFrame { image, transform }
}

macro_rules! generate_imaginate_node {
	($($val:ident: $t:ident: $o:ty,)*) => {
		pub struct ImaginateNode<P: Pixel, E, C, $($t,)*> {
			editor_api: E,
			controller: C,
			$($val: $t,)*
			cache: std::sync::Mutex<HashMap<u64, Image<P>>>,
		}

		impl<'e, P: Pixel, E, C, $($t,)*> ImaginateNode<P, E, C, $($t,)*>
		where $($t: for<'any_input> Node<'any_input, (), Output = DynFuture<'any_input, $o>>,)*
			E: for<'any_input> Node<'any_input, (), Output = DynFuture<'any_input, WasmEditorApi<'e>>>,
			C: for<'any_input> Node<'any_input, (), Output = DynFuture<'any_input, ImaginateController>>,
		{
			#[allow(clippy::too_many_arguments)]
			pub fn new(editor_api: E, controller: C, $($val: $t,)* ) -> Self {
				Self { editor_api, controller, $($val,)* cache: Default::default() }
			}
		}

		impl<'i, 'e: 'i, P: Pixel + 'i + Hash + Default, E: 'i, C: 'i, $($t: 'i,)*> Node<'i, ImageFrame<P>> for ImaginateNode<P, E, C, $($t,)*>
		where $($t: for<'any_input> Node<'any_input, (), Output = DynFuture<'any_input, $o>>,)*
			E: for<'any_input> Node<'any_input, (), Output = DynFuture<'any_input, WasmEditorApi<'e>>>,
			C: for<'any_input> Node<'any_input, (), Output = DynFuture<'any_input, ImaginateController>>,
		{
			type Output = DynFuture<'i, ImageFrame<P>>;

			fn eval(&'i self, frame: ImageFrame<P>) -> Self::Output {
				let controller = self.controller.eval(());
				$(let $val = self.$val.eval(());)*

				use std::hash::Hasher;
				let mut hasher = rustc_hash::FxHasher::default();
				frame.image.hash(&mut hasher);
				let hash =hasher.finish();

				Box::pin(async move {
					let controller: std::pin::Pin<Box<dyn std::future::Future<Output = ImaginateController>>> = controller;
					let controller: ImaginateController = controller.await;
					if controller.take_regenerate_trigger() {
						let editor_api = self.editor_api.eval(());
						let image = super::imaginate::imaginate(frame.image, editor_api, controller, $($val,)*).await;

						self.cache.lock().unwrap().insert(hash, image.clone());
						return ImageFrame {
							image,
							..frame
						}
					}
					let image = self.cache.lock().unwrap().get(&hash).cloned().unwrap_or_default();
					ImageFrame {
						image,
						..frame
					}
				})
			}
		}
	}
}

generate_imaginate_node! {
	seed: Seed: f64,
	res: Res: Option<DVec2>,
	samples: Samples: u32,
	sampling_method: SamplingMethod: ImaginateSamplingMethod,
	prompt_guidance: PromptGuidance: f32,
	prompt: Prompt: String,
	negative_prompt: NegativePrompt: String,
	adapt_input_image: AdaptInputImage: bool,
	image_creativity: ImageCreativity: f32,
	masking_layer: MaskingLayer: Option<Vec<u64>>,
	inpaint: Inpaint: bool,
	mask_blur: MaskBlur: f32,
	mask_starting_fill: MaskStartingFill: ImaginateMaskStartingFill,
	improve_faces: ImproveFaces: bool,
	tiling: Tiling: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ImageFrameNode<P, Transform> {
	transform: Transform,
	_p: PhantomData<P>,
}
#[node_macro::node_fn(ImageFrameNode<_P>)]
fn image_frame<_P: Pixel>(image: Image<_P>, transform: DAffine2) -> graphene_core::raster::ImageFrame<_P> {
	graphene_core::raster::ImageFrame { image, transform }
}
#[cfg(test)]
mod test {

	#[test]
	fn load_image() {
		// TODO: reenable this test
		/*
		let image = image_node::<&str>();

		let grayscale_picture = image.then(MapResultNode::new(&image));
		let export = export_image_node();

		let picture = grayscale_picture.eval("test-image-1.png").expect("Failed to load image");
		export.eval((picture, "test-image-1-result.png")).unwrap();
		*/
	}
}
