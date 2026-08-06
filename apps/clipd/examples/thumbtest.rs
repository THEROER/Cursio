//! Reproduce the truncated-thumbnail bug through the real Store.
fn main() {
    let dir = std::env::temp_dir().join("clipd-thumbtest");
    let _ = std::fs::remove_dir_all(&dir);
    let mut store = clipd::store::Store::open(&dir).unwrap();

    let img = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([9, 80, 200, 255]));
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    println!("source: {} bytes", png.len());

    let content =
        clipboard_core::ClipboardContent::single(clipboard_core::ClipboardFormat::ImagePng, png);
    let id = store.insert(&content).unwrap();

    let item = store.recent(1).unwrap().into_iter().next().unwrap();
    match item.thumbnail {
        Some(p) => {
            let len = std::fs::metadata(&p).unwrap().len();
            println!("thumbnail id={id}: {len} bytes -> {}", p.display());
            if len < 100 {
                println!("!!! TRUNCATED");
            } else {
                println!("ok");
            }
        }
        None => println!("!!! no thumbnail"),
    }
}
