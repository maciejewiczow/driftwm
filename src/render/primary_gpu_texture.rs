use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use crate::backend::udev::{UdevFrame, UdevRenderer, UdevRendererError};

/// Wrapper for a texture from the primary GPU so it can be rendered via
/// the multi-GPU [`UdevRenderer`] path.
///
/// `TextureRenderElement<GlesTexture>` implements `RenderElement<R>` only when
/// `R::TextureId = GlesTexture`, which is true for `GlesRenderer` but not for
/// `UdevRenderer` (whose texture type is the multigpu wrapper). This newtype
/// bridges that gap: its `RenderElement<UdevRenderer>` impl casts through
/// `frame.as_mut()` to reach the underlying `GlesFrame` and delegates to the
/// `GlesRenderer` draw path directly.
#[derive(Debug)]
pub struct PrimaryGpuTextureRenderElement(pub TextureRenderElement<GlesTexture>);

impl Element for PrimaryGpuTextureRenderElement {
    fn id(&self) -> &Id {
        self.0.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.0.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.0.src()
    }

    fn transform(&self) -> Transform {
        self.0.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(scale)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.0.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.0.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.0.alpha()
    }

    fn kind(&self) -> Kind {
        self.0.kind()
    }
}

impl RenderElement<GlesRenderer> for PrimaryGpuTextureRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        RenderElement::<GlesRenderer>::draw(&self.0, frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<UdevRenderer<'render>> for PrimaryGpuTextureRenderElement {
    fn draw(
        &self,
        frame: &mut UdevFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), UdevRendererError<'render>> {
        RenderElement::<GlesRenderer>::draw(
            &self.0,
            frame.as_mut(),
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        _renderer: &mut UdevRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        // Primary-GPU textures are not directly scanout-eligible on a remote
        // output device, so skip the underlying storage path.
        None
    }

    fn capture_framebuffer(
        &self,
        frame: &mut UdevFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), UdevRendererError<'render>> {
        RenderElement::<GlesRenderer>::capture_framebuffer(
            &self.0,
            frame.as_mut(),
            src,
            dst,
            cache,
        )?;
        Ok(())
    }
}
