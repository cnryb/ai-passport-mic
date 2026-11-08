// 开发辅助：把整屏渲染结果存成 PNG，方便在主机上核对排版。
// 不属于固件、不参与门禁，只在需要"看一眼画对了没"时手动跑。
//
//   cc -std=c11 -I device/main device/main/screen_render.c \
//      device/test/preview_screen.c -o /tmp/preview && /tmp/preview /tmp/screen.ppm
//
// 输出 PPM（无压缩、无依赖）；macOS 上可用 sips 转 PNG。
#include "screen_render.h"

#include <stdio.h>
#include <stdlib.h>

#define W 240
#define H 320

static uint16_t s_px[W * H];

static void rgb565_to_rgb888(uint16_t c, unsigned char *r, unsigned char *g, unsigned char *b)
{
    *r = (unsigned char)(((c >> 11) & 0x1F) * 255 / 31);
    *g = (unsigned char)(((c >> 5) & 0x3F) * 255 / 63);
    *b = (unsigned char)((c & 0x1F) * 255 / 31);
}

int main(int argc, char **argv)
{
    const char *out = argc > 1 ? argv[1] : "screen.ppm";
    // 放大 2 倍，肉眼看 16 px 点阵更清楚（不改变像素内容，只是最近邻放大）。
    const int scale = 2;

    apm_canvas_t c = { s_px, W, H };
    apm_screen_view_t v = {
        .battery_pct = 78,
        .low_battery = false,
        .connected = true,
        .capturing = false,
        .labels = { "ctrl+enter", "esc x2", "开麦 关麦" },
        .capture_btn = 2,
        .hint = "长按UP/DOWN键 息屏",
    };
    apm_screen_draw(&c, &v);

    FILE *f = fopen(out, "wb");
    if (!f) { perror(out); return 1; }
    fprintf(f, "P6\n%d %d\n255\n", W * scale, H * scale);
    for (int y = 0; y < H * scale; y++) {
        for (int x = 0; x < W * scale; x++) {
            unsigned char rgb[3];
            rgb565_to_rgb888(s_px[(y / scale) * W + (x / scale)], &rgb[0], &rgb[1], &rgb[2]);
            fwrite(rgb, 1, 3, f);
        }
    }
    fclose(f);
    printf("wrote %s (%dx%d)\n", out, W * scale, H * scale);
    return 0;
}
