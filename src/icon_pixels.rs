pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    for y in 0..size {
        for x in 0..size {
            let color = pixel_color(x, y, size);
            let index = ((y * size + x) * 4) as usize;
            rgba[index] = to_byte(color[0]);
            rgba[index + 1] = to_byte(color[1]);
            rgba[index + 2] = to_byte(color[2]);
            rgba[index + 3] = to_byte(color[3]);
        }
    }

    rgba
}

fn pixel_color(x: u32, y: u32, size: u32) -> [f32; 4] {
    let samples = 3;
    let mut premul = [0.0f32; 3];
    let mut alpha = 0.0f32;

    for sy in 0..samples {
        for sx in 0..samples {
            let px = (x as f32 + (sx as f32 + 0.5) / samples as f32) / size as f32;
            let py = (y as f32 + (sy as f32 + 0.5) / samples as f32) / size as f32;
            let sample = sample_color(px, py);

            premul[0] += sample[0] * sample[3];
            premul[1] += sample[1] * sample[3];
            premul[2] += sample[2] * sample[3];
            alpha += sample[3];
        }
    }

    let total = (samples * samples) as f32;
    alpha /= total;
    if alpha <= f32::EPSILON {
        return [0.0, 0.0, 0.0, 0.0];
    }

    [
        premul[0] / total / alpha,
        premul[1] / total / alpha,
        premul[2] / total / alpha,
        alpha,
    ]
}

fn sample_color(px: f32, py: f32) -> [f32; 4] {
    let mut color = [0.0, 0.0, 0.0, 0.0];

    if rounded_rect(px, py, 0.055, 0.055, 0.89, 0.89, 0.18) {
        color = over(color, rgba(45, 127, 226, 255));
    }
    if rounded_rect(px, py, 0.095, 0.095, 0.81, 0.81, 0.14) {
        color = over(color, rgba(29, 34, 43, 255));
    }
    if rounded_rect(px, py, 0.275, 0.255, 0.49, 0.58, 0.055) {
        color = over(color, rgba(8, 12, 18, 80));
    }
    if rounded_rect(px, py, 0.245, 0.225, 0.49, 0.58, 0.055) {
        color = over(color, rgba(241, 246, 255, 255));
    }
    if rounded_rect(px, py, 0.345, 0.145, 0.29, 0.17, 0.055) {
        color = over(color, rgba(58, 151, 247, 255));
    }
    if rounded_rect(px, py, 0.385, 0.105, 0.21, 0.08, 0.035) {
        color = over(color, rgba(183, 217, 255, 255));
    }

    for line_y in [0.405, 0.505, 0.605] {
        if rounded_rect(px, py, 0.335, line_y, 0.33, 0.035, 0.018) {
            color = over(color, rgba(83, 97, 118, 210));
        }
    }

    let bolt = [
        (0.58, 0.385),
        (0.42, 0.61),
        (0.535, 0.61),
        (0.47, 0.82),
        (0.705, 0.535),
        (0.575, 0.535),
    ];
    if point_in_polygon(px, py, &bolt) {
        color = over(color, rgba(247, 197, 57, 255));
    }

    color
}

fn rounded_rect(px: f32, py: f32, x: f32, y: f32, width: f32, height: f32, radius: f32) -> bool {
    if px < x || px > x + width || py < y || py > y + height {
        return false;
    }

    let dx = if px < x + radius {
        x + radius - px
    } else if px > x + width - radius {
        px - (x + width - radius)
    } else {
        0.0
    };
    let dy = if py < y + radius {
        y + radius - py
    } else if py > y + height - radius {
        py - (y + height - radius)
    } else {
        0.0
    };

    dx * dx + dy * dy <= radius * radius
}

fn point_in_polygon(px: f32, py: f32, points: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = points.len() - 1;

    for i in 0..points.len() {
        let (xi, yi) = points[i];
        let (xj, yj) = points[j];
        let crosses = (yi > py) != (yj > py);
        if crosses {
            let x_at_y = (xj - xi) * (py - yi) / (yj - yi) + xi;
            if px < x_at_y {
                inside = !inside;
            }
        }
        j = i;
    }

    inside
}

fn over(base: [f32; 4], top: [f32; 4]) -> [f32; 4] {
    let out_alpha = top[3] + base[3] * (1.0 - top[3]);
    if out_alpha <= f32::EPSILON {
        return [0.0, 0.0, 0.0, 0.0];
    }

    [
        (top[0] * top[3] + base[0] * base[3] * (1.0 - top[3])) / out_alpha,
        (top[1] * top[3] + base[1] * base[3] * (1.0 - top[3])) / out_alpha,
        (top[2] * top[3] + base[2] * base[3] * (1.0 - top[3])) / out_alpha,
        out_alpha,
    ]
}

fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> [f32; 4] {
    [
        red as f32 / 255.0,
        green as f32 / 255.0,
        blue as f32 / 255.0,
        alpha as f32 / 255.0,
    ]
}

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}
