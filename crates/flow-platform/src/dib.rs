//! Windows 剪贴板位图（`CF_DIB`）和 PNG 互转。纯字节处理，不调用系统 API。
//!
//! 流界在设备之间一律传 PNG；Windows 上截图、画图等程序放进剪贴板的是 `CF_DIB`
//! （`BITMAPINFOHEADER` + 像素），写入时也要给一份 `CF_DIB`，大多数程序只认它。

/// 宽或高超过这么多像素的图片不处理。
const MAX_SIDE: u32 = 32_768;
/// 解码后的像素最多占这么多字节，防止很小的 PNG 解出巨大的图把内存占满。
const MAX_PIXEL_BYTES: usize = 512 * 1024 * 1024;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const HEADER_LEN: usize = 40;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// 颜色掩码对应的右移位数；只支持 8 位一个通道。
fn shift_of(mask: u32) -> Option<u32> {
    let shift = mask.trailing_zeros();
    (mask != 0 && mask >> shift == 0xFF).then_some(shift)
}

/// `CF_DIB` 数据转 PNG。只支持 24 位和 32 位未压缩位图（截图和常见程序复制的图片都是），
/// 其他格式返回 `None`。透明度不保留：32 位 `BI_RGB` 的第 4 个字节没有统一含义。
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let header = u32_at(dib, 0)? as usize;
    if header < HEADER_LEN {
        return None;
    }
    let width = u32_at(dib, 4)? as i32;
    let raw_height = u32_at(dib, 8)? as i32;
    let bits = u16_at(dib, 14)?;
    let compression = u32_at(dib, 16)?;
    let colors = u32_at(dib, 32)? as usize;
    // 高度为负表示第一行在最上面；通常为正，第一行在最下面。
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    if width <= 0 || height == 0 || width as u32 > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let width = width as usize;
    let height = height as usize;
    let bpp = match bits {
        24 => 3,
        32 => 4,
        _ => return None,
    };
    // 每个通道在像素里的右移位数（按小端 32 位读）。
    let (r, g, b, masks_len) = match compression {
        BI_RGB => (16, 8, 0, 0),
        BI_BITFIELDS if bits == 32 => {
            // 40 字节的头后面跟三个掩码；更长的头（V4/V5）里掩码就在头里，位置相同。
            let masks_len = if header == HEADER_LEN { 12 } else { 0 };
            (
                shift_of(u32_at(dib, 40)?)?,
                shift_of(u32_at(dib, 44)?)?,
                shift_of(u32_at(dib, 48)?)?,
                masks_len,
            )
        }
        _ => return None,
    };
    let stride = (width * bits as usize).div_ceil(32) * 4;
    let offset = header
        .checked_add(masks_len)?
        .checked_add(colors.checked_mul(4)?)?;
    let pixels = dib.get(offset..offset.checked_add(stride.checked_mul(height)?)?)?;

    let mut rgb = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        let row = if top_down { y } else { height - 1 - y };
        let line = &pixels[row * stride..row * stride + width * bpp];
        for px in line.chunks_exact(bpp) {
            let v = u32::from_le_bytes([px[0], px[1], px[2], if bpp == 4 { px[3] } else { 0 }]);
            rgb.extend_from_slice(&[(v >> r) as u8, (v >> g) as u8, (v >> b) as u8]);
        }
    }

    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    // 截图马上要用：压缩快比文件小重要。
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&rgb).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// PNG 转 `CF_DIB` 数据（32 位、自下而上、`BI_RGB`）。透明的地方垫白色：
/// 只认 `CF_DIB` 的程序（画图、多数聊天软件）不处理透明度。无法解码时返回 `None`。
pub fn png_to_dib(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    // 调色板、灰度位深、16 位一律展开成 8 位通道。
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_limits(png::Limits {
        bytes: MAX_PIXEL_BYTES,
    });
    let mut reader = decoder.read_info().ok()?;
    let size = reader.output_buffer_size()?;
    if size > MAX_PIXEL_BYTES {
        return None;
    }
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.width == 0 || info.height == 0 || info.width > MAX_SIDE || info.height > MAX_SIDE {
        return None;
    }
    let (width, height) = (info.width as usize, info.height as usize);
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
    };
    let src = buf.get(..width * height * channels)?;

    let mut out = Vec::with_capacity(HEADER_LEN + width * height * 4);
    out.extend_from_slice(&(HEADER_LEN as u32).to_le_bytes()); // biSize
    out.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight（正数：自下而上）
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&BI_RGB.to_le_bytes()); // biCompression
    out.extend_from_slice(&((width * height * 4) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0; 16]); // 分辨率、调色板：不用
    // 垫白色：c * a + 255 * (1 - a)。
    let over_white =
        |c: u8, a: u8| ((c as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8;
    for y in (0..height).rev() {
        let line = &src[y * width * channels..(y + 1) * width * channels];
        for px in line.chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                1 => (px[0], px[0], px[0], 255),
                2 => (px[0], px[0], px[0], px[1]),
                3 => (px[0], px[1], px[2], 255),
                _ => (px[0], px[1], px[2], px[3]),
            };
            out.extend_from_slice(&[over_white(b, a), over_white(g, a), over_white(r, a), 255]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 的 24 位位图（每行补齐到 4 字节），自下而上：下面一行蓝、白，上面一行红、绿。
    fn dib_24() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0; 24]);
        d.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]); // 蓝、白 + 补齐
        d.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // 红、绿 + 补齐
        d
    }

    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[test]
    fn dib_becomes_png_with_rows_flipped() {
        let png = dib_to_png(&dib_24()).unwrap();
        let (w, h, rgb) = decode(&png);
        assert_eq!((w, h), (2, 2));
        // PNG 第一行是最上面一行：红、绿；然后蓝、白。
        assert_eq!(rgb, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
    }

    #[test]
    fn top_down_and_bitfields_dibs() {
        // 1×2、32 位、自上而下（高度为负）、带掩码（RGBA 顺序，和默认的 BGRA 相反）。
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&1i32.to_le_bytes());
        d.extend_from_slice(&(-2i32).to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&32u16.to_le_bytes());
        d.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
        d.extend_from_slice(&[0; 20]);
        for mask in [0x0000_00FFu32, 0x0000_FF00, 0x00FF_0000] {
            d.extend_from_slice(&mask.to_le_bytes());
        }
        d.extend_from_slice(&[10, 20, 30, 0]);
        d.extend_from_slice(&[40, 50, 60, 0]);
        let (w, h, rgb) = decode(&dib_to_png(&d).unwrap());
        assert_eq!((w, h), (1, 2));
        assert_eq!(rgb, [10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn png_round_trips_through_dib() {
        let png = dib_to_png(&dib_24()).unwrap();
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(u32_at(&dib, 0), Some(40));
        assert_eq!(u16_at(&dib, 14), Some(32));
        assert_eq!(dib.len(), 40 + 2 * 2 * 4);
        // 再转回来，像素不变。
        let (_, _, rgb) = decode(&dib_to_png(&dib).unwrap());
        assert_eq!(rgb, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
    }

    #[test]
    fn transparent_pixels_become_white() {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        // 全透明的黑、不透明的红。
        writer
            .write_image_data(&[0, 0, 0, 0, 255, 0, 0, 255])
            .unwrap();
        writer.finish().unwrap();
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(&dib[40..], [255, 255, 255, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn bad_data_is_rejected() {
        assert_eq!(dib_to_png(&[1, 2, 3]), None);
        assert_eq!(png_to_dib(b"not a png"), None);
        // 像素数据比头里声明的少。
        let mut short = dib_24();
        short.truncate(50);
        assert_eq!(dib_to_png(&short), None);
        // 8 位调色板位图不支持。
        let mut paletted = dib_24();
        paletted[14] = 8;
        assert_eq!(dib_to_png(&paletted), None);
        // 声明的尺寸大得离谱。
        let mut huge = dib_24();
        huge[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
        assert_eq!(dib_to_png(&huge), None);
    }
}
