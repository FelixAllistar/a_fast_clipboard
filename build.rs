fn main() {
    println!("cargo:rerun-if-changed=src/icon_pixels.rs");
    slint_build::compile("ui/appwindow.slint").expect("failed to compile Slint UI");
    embed_windows_resources();
}

#[cfg(target_os = "windows")]
mod icon_pixels {
    include!("src/icon_pixels.rs");
}

#[cfg(target_os = "windows")]
fn embed_windows_resources() {
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let icon_path = out_dir.join("app.ico");
    write_app_icon(&icon_path).expect("failed to write app icon");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon_path.to_str().expect("icon path is utf-8"));
    resource.set("FileDescription", "Fast Clipboard");
    resource.set("ProductName", "Fast Clipboard");
    resource.set("OriginalFilename", "a_fast_clipboard.exe");
    resource
        .compile()
        .expect("failed to embed Windows resources");
}

#[cfg(not(target_os = "windows"))]
fn embed_windows_resources() {}

#[cfg(target_os = "windows")]
fn write_app_icon(path: &std::path::Path) -> std::io::Result<()> {
    let images = [16u32, 32, 48, 64, 128]
        .into_iter()
        .map(|size| (size, dib_icon_image(size)))
        .collect::<Vec<_>>();
    let mut icon = Vec::new();

    push_u16(&mut icon, 0);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, images.len() as u16);

    let mut offset = 6 + images.len() as u32 * 16;
    for (size, image) in &images {
        icon.push(*size as u8);
        icon.push(*size as u8);
        icon.push(0);
        icon.push(0);
        push_u16(&mut icon, 1);
        push_u16(&mut icon, 32);
        push_u32(&mut icon, image.len() as u32);
        push_u32(&mut icon, offset);
        offset += image.len() as u32;
    }

    for (_, image) in images {
        icon.extend_from_slice(&image);
    }

    std::fs::write(path, icon)
}

#[cfg(target_os = "windows")]
fn dib_icon_image(size: u32) -> Vec<u8> {
    let mut image = Vec::new();

    push_u32(&mut image, 40);
    push_i32(&mut image, size as i32);
    push_i32(&mut image, (size * 2) as i32);
    push_u16(&mut image, 1);
    push_u16(&mut image, 32);
    push_u32(&mut image, 0);
    push_u32(&mut image, size * size * 4);
    push_i32(&mut image, 0);
    push_i32(&mut image, 0);
    push_u32(&mut image, 0);
    push_u32(&mut image, 0);

    let rgba = icon_pixels::app_icon_rgba(size);
    for y in (0..size).rev() {
        for x in 0..size {
            let index = ((y * size + x) * 4) as usize;
            image.push(rgba[index + 2]);
            image.push(rgba[index + 1]);
            image.push(rgba[index]);
            image.push(rgba[index + 3]);
        }
    }

    let mask_stride = size.div_ceil(32) * 4;
    image.resize(image.len() + (mask_stride * size) as usize, 0);
    image
}

#[cfg(target_os = "windows")]
fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

#[cfg(target_os = "windows")]
fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

#[cfg(target_os = "windows")]
fn push_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
