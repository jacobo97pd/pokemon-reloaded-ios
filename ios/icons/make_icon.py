"""Draws the Reloaded app icon: an original pixel-art critter peeking out of tall grass.

Usage: python make_icon.py <out_dir>
Writes the iOS icon sizes plus a 1024 master and a preview.
"""
import math
import os
import sys

from PIL import Image

N = 30  # grid size; 120 and 180 (the iPhone sizes) are exact multiples
C = 15.0  # vertical symmetry axis

SKY = [(79, 168, 240), (104, 186, 246), (132, 204, 250), (168, 222, 255)]
CLOUD = (255, 255, 255)
CLOUD_SHADE = (214, 236, 255)
HILL = (109, 191, 139)
HILL_DARK = (86, 166, 120)
OUTLINE = (42, 45, 74)
BODY = (92, 207, 192)
BODY_SHADE = (59, 167, 155)
BODY_LIGHT = (169, 239, 228)
EAR_INNER = (255, 212, 92)
EYE = (30, 27, 51)
EYE_SHINE = (255, 255, 255)
BLUSH = (255, 143, 168)
GRASS_EDGE = (31, 107, 46)
GRASS = (77, 184, 72)
GRASS_LIGHT = (124, 214, 96)
GRASS_DEEP = (58, 154, 64)


def centre(x, y):
    return x + 0.5, y + 0.5


def in_triangle(p, a, b, c):
    def sign(p1, p2, p3):
        return (p1[0] - p3[0]) * (p2[1] - p3[1]) - (p2[0] - p3[0]) * (p1[1] - p3[1])

    d1, d2, d3 = sign(p, a, b), sign(p, b, c), sign(p, c, a)
    neg = d1 < 0 or d2 < 0 or d3 < 0
    pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (neg and pos)


def mirror(pt):
    return 2 * C - pt[0], pt[1]


HEAD_C = (C, 15.2)
HEAD_R = 8.6
EAR_L = ((6.2, 11.0), (6.0, 1.6), (13.4, 7.4))
EAR_L_IN = ((7.6, 9.4), (7.4, 4.2), (11.6, 7.6))
EAR_R = tuple(mirror(p) for p in EAR_L)
EAR_R_IN = tuple(mirror(p) for p in EAR_L_IN)

BLADES = [(0.5, 15), (3.5, 18), (6.5, 14.5), (9.5, 19), (12.5, 20.5), (15.0, 21.5)]
BLADES += [(2 * C - cx, ty) for cx, ty in BLADES if cx != C]
GRASS_BASE = 24


def head(x, y):
    px, py = centre(x, y)
    return math.hypot(px - HEAD_C[0], py - HEAD_C[1]) <= HEAD_R


def ear(x, y):
    p = centre(x, y)
    return in_triangle(p, *EAR_L) or in_triangle(p, *EAR_R)


def ear_inner(x, y):
    p = centre(x, y)
    return in_triangle(p, *EAR_L_IN) or in_triangle(p, *EAR_R_IN)


def critter(x, y):
    return 0 <= x < N and 0 <= y < N and (head(x, y) or ear(x, y))


def grass(x, y):
    if not (0 <= x < N and 0 <= y < N):
        return y >= GRASS_BASE
    if y >= GRASS_BASE:
        return True
    px, py = centre(x, y)
    return any(py >= ty + 2.2 * abs(px - cx) for cx, ty in BLADES)


def cloud(x, y):
    px, py = centre(x, y)
    clouds = [
        (5.2, [((1.6, 4.0), 1.7), ((3.6, 3.2), 2.0), ((5.4, 4.3), 1.2)]),
        (11.2, [((25.6, 10.0), 1.6), ((27.6, 9.0), 2.1), ((29.6, 10.2), 1.4)]),
    ]
    for bottom, blobs in clouds:
        if py <= bottom and any(math.hypot(px - bx, py - by) <= r for (bx, by), r in blobs):
            return True
    return False


def draw():
    img = Image.new("RGB", (N, N))
    put = img.putpixel

    # sky in flat bands, as old handheld games did
    for y in range(N):
        band = SKY[min(len(SKY) - 1, y * len(SKY) // 20)]
        for x in range(N):
            put((x, y), band)

    for y in range(N):
        for x in range(N):
            if cloud(x, y):
                put((x, y), CLOUD_SHADE if not cloud(x, y + 1) else CLOUD)

    # distant hills
    for x in range(N):
        top = 18 + round(1.6 * math.sin((x + 0.5) * 0.45))
        for y in range(top, N):
            put((x, y), HILL_DARK if y == top else HILL)

    # critter: outline, body, shading
    for y in range(N):
        for x in range(N):
            if critter(x, y):
                continue
            if any(critter(x + dx, y + dy) for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1))):
                put((x, y), OUTLINE)
    for y in range(N):
        for x in range(N):
            if not critter(x, y):
                continue
            px, py = centre(x, y)
            dx, dy = px - HEAD_C[0], py - HEAD_C[1]
            d = math.hypot(dx, dy)
            colour = BODY
            if head(x, y):
                if d > HEAD_R - 1.7 and dx + dy > 4.0:
                    colour = BODY_SHADE
                elif HEAD_R - 2.2 < d < HEAD_R - 1.0 and dx < -2.5 and dy < -3.0:
                    colour = BODY_LIGHT
            if ear_inner(x, y) and not head(x, y):
                colour = EAR_INNER
            put((x, y), colour)

    # face
    for ex in (10, 17):
        for y in range(13, 17):
            for x in range(ex, ex + 3):
                put((x, y), EYE)
        put((ex, 13), EYE_SHINE)
    for x, y in ((8, 17), (9, 17), (20, 17), (21, 17)):
        put((x, y), BLUSH)
    for x, y in ((13, 18), (14, 19), (15, 19), (16, 18)):
        put((x, y), OUTLINE)

    # tall grass in front
    for y in range(N):
        for x in range(N):
            if not grass(x, y):
                continue
            edge = not grass(x, y - 1) or not grass(x - 1, y) or not grass(x + 1, y)
            if edge and y < GRASS_BASE + 1:
                colour = GRASS_EDGE
            elif y >= 27:
                colour = GRASS_DEEP
            elif (x + y // 3) % 4 == 0:
                colour = GRASS_LIGHT
            else:
                colour = GRASS
            put((x, y), colour)
    return img


def scaled(img, size):
    if size % N == 0:
        return img.resize((size, size), Image.NEAREST)
    big = N * math.ceil(size / N) * 2
    return img.resize((big, big), Image.NEAREST).resize((size, size), Image.LANCZOS)


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    img = draw()
    files = {
        "AppIcon60x60@2x.png": 120,
        "AppIcon60x60@3x.png": 180,
        "AppIcon76x76@2x~ipad.png": 152,
        "AppIcon83.5x83.5@2x~ipad.png": 167,
        "icon-1024.png": 1024,
    }
    for name, size in files.items():
        scaled(img, size).save(os.path.join(out, name), optimize=True)
    print("ok", out)


if __name__ == "__main__":
    main()
