//! Booru-style image boards outside the dedicated `crate::booru` module.
use crate::sites::Site;

mod moebooru;

pub(super) static SITES: &[&Site] = &[
    &moebooru::YANDERE,
    &moebooru::KONACHAN,
    &moebooru::SAKUGABOORU,
    &moebooru::LOLIBOORU,
];
