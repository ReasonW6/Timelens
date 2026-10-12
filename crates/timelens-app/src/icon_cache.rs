use slint::Image;
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

pub(crate) const ICON_CACHE_BYTES: usize = 12 * 1024 * 1024;
const MAX_ALIASES: usize = 256;

struct Alias {
    identity: String,
    loaded: Instant,
}
struct Icon {
    source: Image,
    fingerprint: Option<u64>,
    aliases: VecDeque<Alias>,
    sizes: Vec<(u32, Image)>,
}

/// Original and DPI variants share one owner and one LRU byte budget.
pub(crate) struct IconCache {
    icons: VecDeque<Icon>,
    budget: usize,
}

impl Default for IconCache {
    fn default() -> Self {
        Self {
            icons: VecDeque::new(),
            budget: ICON_CACHE_BYTES,
        }
    }
}

fn bytes(image: &Image) -> usize {
    let size = image.size();
    size.width as usize * size.height as usize * 4
}

fn fingerprint(image: &Image) -> Option<u64> {
    let pixels = image.to_rgba8_premultiplied()?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    pixels.width().hash(&mut hash);
    pixels.height().hash(&mut hash);
    pixels.as_bytes().hash(&mut hash);
    Some(hash.finish())
}

impl IconCache {
    pub(crate) fn used_bytes(&self) -> usize {
        self.icons
            .iter()
            .map(|icon| {
                bytes(&icon.source)
                    + icon
                        .sizes
                        .iter()
                        .filter(|(_, image)| *image != icon.source)
                        .map(|(_, image)| bytes(image))
                        .sum::<usize>()
            })
            .sum()
    }

    fn enforce(&mut self) {
        while self.used_bytes() > self.budget
            || self
                .icons
                .iter()
                .map(|icon| icon.aliases.len())
                .sum::<usize>()
                > MAX_ALIASES
        {
            if self.icons.len() == 1 && self.used_bytes() <= self.budget {
                self.icons[0].aliases.pop_back();
            } else {
                self.icons.pop_back();
            }
        }
    }

    pub(crate) fn invalidate(&mut self, identity: &str) {
        self.icons.retain_mut(|icon| {
            let found = icon.aliases.iter().any(|alias| alias.identity == identity);
            icon.aliases.retain(|alias| alias.identity != identity);
            !found || !icon.aliases.is_empty()
        });
    }

    pub(crate) fn get(&mut self, identity: &str, now: Instant) -> Option<Image> {
        let index = self
            .icons
            .iter()
            .position(|icon| icon.aliases.iter().any(|alias| alias.identity == identity))?;
        let mut icon = self.icons.remove(index)?;
        let alias = icon
            .aliases
            .iter()
            .position(|alias| alias.identity == identity)?;
        if icon.source.size().width == 0
            && now.duration_since(icon.aliases[alias].loaded) >= Duration::from_secs(30)
        {
            icon.aliases.remove(alias);
            if !icon.aliases.is_empty() {
                self.icons.push_front(icon);
            }
            return None;
        }
        let alias = icon.aliases.remove(alias)?;
        icon.aliases.push_front(alias);
        let image = icon.source.clone();
        self.icons.push_front(icon);
        Some(image)
    }

    fn source_index(&self, image: &Image, key: Option<u64>) -> Option<usize> {
        self.icons.iter().position(|icon| {
            icon.source == *image
                || (key.is_some()
                    && icon.fingerprint == key
                    && icon.source.size() == image.size()
                    && icon
                        .source
                        .to_rgba8_premultiplied()
                        .zip(image.to_rgba8_premultiplied())
                        .is_some_and(|(a, b)| a.as_bytes() == b.as_bytes()))
        })
    }

    pub(crate) fn insert(&mut self, identity: &str, image: Image, now: Instant) -> Image {
        self.invalidate(identity);
        let key = fingerprint(&image);
        let mut icon = self
            .source_index(&image, key)
            .and_then(|index| self.icons.remove(index))
            .unwrap_or_else(|| Icon {
                source: image,
                fingerprint: key,
                aliases: VecDeque::new(),
                sizes: Vec::new(),
            });
        icon.aliases.push_front(Alias {
            identity: identity.to_owned(),
            loaded: now,
        });
        let image = icon.source.clone();
        self.icons.push_front(icon);
        self.enforce();
        image
    }

    pub(crate) fn get_size(&mut self, source: &Image, size: u32) -> Option<Image> {
        let index = self.icons.iter().position(|icon| icon.source == *source)?;
        let icon = self.icons.remove(index)?;
        let image = icon
            .sizes
            .iter()
            .find(|(pixels, _)| *pixels == size)
            .map(|(_, image)| image.clone());
        self.icons.push_front(icon);
        image
    }

    pub(crate) fn insert_size(&mut self, source: Image, size: u32, image: Image) {
        let index = self.icons.iter().position(|icon| icon.source == source);
        let mut icon = index
            .and_then(|index| self.icons.remove(index))
            .unwrap_or_else(|| Icon {
                fingerprint: fingerprint(&source),
                source,
                aliases: VecDeque::new(),
                sizes: Vec::new(),
            });
        if !icon.sizes.iter().any(|(pixels, _)| *pixels == size) {
            icon.sizes.push((size, image));
        }
        self.icons.push_front(icon);
        self.enforce();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    fn image(size: u32, red: u8) -> Image {
        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
        pixels.make_mut_slice().fill(Rgba8Pixel {
            r: red,
            g: 0,
            b: 0,
            a: 255,
        });
        Image::from_rgba8(pixels)
    }

    #[test]
    fn originals_and_dpi_variants_share_a_budget_and_evict_together() {
        let now = Instant::now();
        let mut cache = IconCache {
            budget: 9000,
            ..Default::default()
        };
        let first = cache.insert("first", image(32, 10), now);
        cache.insert("second", image(32, 20), now);
        assert!(cache.get("first", now).is_some());
        cache.insert_size(first.clone(), 16, image(16, 10));
        assert!(cache.used_bytes() <= 9000);
        assert!(cache.get("second", now).is_none());
        assert!(cache.get_size(&first, 16).is_some());
        cache.invalidate("first");
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn identical_rasters_are_deduplicated_and_failures_still_retry() {
        let now = Instant::now();
        let mut cache = IconCache::default();
        let first = cache.insert("one", image(32, 10), now);
        let second = cache.insert("two", image(32, 10), now);
        assert_eq!(first, second);
        assert_eq!(cache.used_bytes(), 32 * 32 * 4);
        cache.insert("missing", Image::default(), now);
        assert!(
            cache
                .get("missing", now + Duration::from_secs(29))
                .is_some()
        );
        assert!(
            cache
                .get("missing", now + Duration::from_secs(30))
                .is_none()
        );
        for index in 0..300 {
            cache.insert(&format!("empty{index}"), Image::default(), now);
        }
        assert!(
            cache
                .icons
                .iter()
                .map(|icon| icon.aliases.len())
                .sum::<usize>()
                <= MAX_ALIASES
        );
    }
}
