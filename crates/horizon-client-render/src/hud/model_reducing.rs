//! Resolution fallback for model textures that exceed the shared HUD atlas.
use super::{HashMap, HashSet, HudAtlas, PackedImage, Path, Result, bitmap_font_textures};

/// Only model atlas sprites may lose resolution. Font sheets can also be
/// model textures, so protect both their file names and renamed sources.
pub(super) fn reducible_model_sprites(
    root: &Path,
    images: &[PackedImage],
    bordered: &HashSet<String>,
) -> HashSet<String> {
    use crate::atlas_sources::{self, SpriteOrigin};
    let fonts: HashSet<_> = bitmap_font_textures(root).into_iter().collect();
    let protected = |name: &str| {
        let path = name.split_once(':').map_or(name, |(_, path)| path);
        path.starts_with("gui/")
            || path.starts_with("font/")
            || fonts.contains(&atlas_sources::texture_path(root, name, "textures"))
    };
    let mut models: HashSet<String> = images
        .iter()
        .filter(|(name, ..)| name.starts_with("item/") || name.starts_with("block/"))
        .map(|(name, ..)| name.clone())
        .collect();
    let textures = atlas_sources::list_textures(root);
    let mut protected_aliases = HashSet::new();
    for (atlas, sources) in atlas_sources::atlases(root) {
        if atlas != "minecraft:blocks" && atlas != "minecraft:items" {
            continue;
        }
        for (id, origin) in atlas_sources::list(&sources, &textures) {
            let name = id.strip_prefix("minecraft:").unwrap_or(&id).to_owned();
            if let SpriteOrigin::File(texture) = origin {
                if protected(&texture) {
                    protected_aliases.insert(name);
                } else {
                    models.insert(name);
                }
            }
        }
    }
    models.retain(|name| {
        !bordered.contains(name) && !protected(name) && !protected_aliases.contains(name)
    });
    models
}

/// Every sprite that may lose resolution when even the model textures' fallback leaves the
/// atlas too big for the GPU (`HudAtlas::layout_reducing_largest`): all but the fonts,
/// Horizon's sprites and blurred (bordered) ones, so text stays sharp. GUI sprites shrink
/// only after other textures half their size (`shrink_order`).
pub(super) fn reducible_sprites(
    root: &Path,
    images: &[PackedImage],
    bordered: &HashSet<String>,
) -> HashSet<String> {
    use crate::atlas_sources;
    let fonts: HashSet<_> = bitmap_font_textures(root).into_iter().collect();
    images
        .iter()
        .map(|(name, ..)| name)
        .filter(|name| {
            let path = name.split_once(':').map_or(name.as_str(), |(_, path)| path);
            !(path.starts_with("font/")
                || path.starts_with("horizon/")
                || bordered.contains(*name)
                || fonts.contains(&atlas_sources::texture_path(root, name, "textures")))
        })
        .cloned()
        .collect()
}

/// The order `layout_reducing_largest` halves textures in, largest first: a GUI sprite counts
/// as half its size, so menus lose detail only after the mob skins and paintings twice their
/// size have, and a texture of 64 texels or less as a quarter.
fn shrink_order((name, w, h, _): &PackedImage) -> u32 {
    let path = name.split_once(':').map_or(name.as_str(), |(_, path)| path);
    let side = (*w).max(*h);
    // A 64-texel mob skin draws a pig's eye with one texel: halved, the eye blurs away. They
    // go last, after the GUI.
    if side <= 64 {
        side / 4
    } else if path.starts_with("gui/") {
        side / 2
    } else {
        side
    }
}

/// The texels each kind of texture (`entity`, `gui`, `item`...) takes, largest first: what to
/// look at when an atlas doesn't fit.
pub(super) fn texel_summary(images: &[PackedImage]) -> String {
    let mut kinds: HashMap<&str, (u64, usize)> = HashMap::new();
    for (name, w, h, _) in images {
        let path = name.split_once(':').map_or(name.as_str(), |(_, path)| path);
        let kind = path.split('/').next().unwrap_or(path);
        let entry = kinds.entry(kind).or_default();
        entry.0 += u64::from(*w) * u64::from(*h);
        entry.1 += 1;
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    kinds
        .iter()
        .take(8)
        .map(|(kind, (texels, count))| format!("{kind}: {count} images, {texels} texels"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl HudAtlas {
    /// `layout`, but when `extras` (Custom Item Textures) make the images more than `max`
    /// holds, without them: they are left out, not the packs.
    pub(super) fn layout_leaving_out(
        images: &mut Vec<PackedImage>,
        animated: &mut Vec<String>,
        extras: &HashSet<String>,
        max: u32,
    ) -> Result<(u32, Vec<(u32, u32)>, u32)> {
        match Self::layout(images, max) {
            Err(e) if !extras.is_empty() => {
                images.retain(|i| !extras.contains(&i.0));
                animated.retain(|n| !extras.contains(n));
                let layout = Self::layout(images, max)?;
                log::warn!("left out the custom item textures: {e}");
                Ok(layout)
            }
            layout => layout,
        }
    }
    /// Last resort for a pack that exceeds the shared atlas: halve model
    /// textures until they fit, retaining their logical GUI sizes. Never
    /// shrink run-time slots (empty pixels), or below the vanilla 16 texels.
    pub(super) fn layout_reducing_models(
        images: &mut [PackedImage],
        gui_sizes: &mut HashMap<String, (u32, u32)>,
        models: &HashSet<String>,
        max: u32,
    ) -> Result<(u32, Vec<(u32, u32)>, u32)> {
        loop {
            let error = match Self::layout(images, max) {
                Ok(layout) => return Ok(layout),
                Err(error) => error,
            };
            let mut changed = false;
            for (name, w, h, pixels) in images.iter_mut() {
                if !models.contains(name) || pixels.is_empty() || (*w <= 16 && *h <= 16) {
                    continue;
                }
                let (nw, nh) = ((*w / 2).max((*w).min(16)), (*h / 2).max((*h).min(16)));
                let mut scaled = Vec::with_capacity((nw * nh * 4) as usize);
                for y in 0..nh {
                    for x in 0..nw {
                        let at = (((y * *h / nh) * *w + x * *w / nw) * 4) as usize;
                        scaled.extend_from_slice(&pixels[at..at + 4]);
                    }
                }
                gui_sizes.entry(name.clone()).or_insert((*w, *h));
                *pixels = scaled;
                (*w, *h) = (nw, nh);
                changed = true;
            }
            if !changed {
                return Err(error);
            }
            images.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        }
    }
    /// For a GPU whose textures are smaller than the atlas (the Raspberry Pi 4's V3D allows
    /// 4096 a side): halve the largest `reducible` textures, then the next largest, until the
    /// atlas fits `max`, keeping their logical GUI sizes. Never below 16 texels, never
    /// run-time slots (empty pixels).
    pub(super) fn layout_reducing_largest(
        images: &mut [PackedImage],
        gui_sizes: &mut HashMap<String, (u32, u32)>,
        reducible: &HashSet<String>,
        max: u32,
    ) -> Result<(u32, Vec<(u32, u32)>, u32)> {
        loop {
            let error = match Self::layout(images, max) {
                Ok(layout) => return Ok(layout),
                Err(error) => error,
            };
            let candidate = |(name, w, h, pixels): &PackedImage| {
                reducible.contains(name) && !pixels.is_empty() && (*w).max(*h) > 16
            };
            let Some(largest) = images
                .iter()
                .filter(|i| candidate(i))
                .map(shrink_order)
                .max()
            else {
                return Err(error);
            };
            for image in images.iter_mut() {
                if !candidate(image) || shrink_order(image) != largest {
                    continue;
                }
                let (name, w, h, pixels) = image;
                let (nw, nh) = ((*w / 2).max(1), (*h / 2).max(1));
                *pixels = super::shrink_rgba(pixels, *w, *h, 2);
                gui_sizes.entry(name.clone()).or_insert((*w, *h));
                (*w, *h) = (nw, nh);
            }
            images.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        }
    }
}
