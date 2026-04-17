"""Sample Python file for complexity analysis."""

MAX_SIZE = 256


def parse_header(buf: bytes) -> int:
    if not buf:
        return -1
    total = 0
    for b in buf:
        if b == ord('\n'):
            continue
        if ord('0') <= b <= ord('9'):
            total = total * 10 + (b - ord('0'))
        else:
            raise ValueError(f"parse error at {total}")
    return total


def dispatch(op: int, x: int) -> int:
    if op == 1:
        return x + 1
    elif op == 2:
        return x * 2
    elif op == 3:
        return x - 1
    else:
        return 0


def crc32_small(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for d in data:
        crc ^= d
        for _ in range(8):
            if crc & 1:
                crc = (crc >> 1) ^ 0xEDB88320
            else:
                crc >>= 1
    return (~crc) & 0xFFFFFFFF


class Counter:
    def __init__(self):
        self.n = 0

    def inc(self, by: int) -> int:
        if by < 0:
            raise ValueError("neg")
        self.n += by
        return self.n
