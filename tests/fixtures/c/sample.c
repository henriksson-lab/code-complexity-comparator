/* Sample C file for complexity analysis. */

#include <stdio.h>
#include <string.h>

#define MAX_SIZE 256

int parse_header(const char *buf, int len) {
    if (buf == NULL || len <= 0) {
        return -1;  // invalid
    }

    int total = 0;
    for (int i = 0; i < len; i++) {
        if (buf[i] == '\n') {
            continue;
        }
        if (buf[i] >= '0' && buf[i] <= '9') {
            total = total * 10 + (buf[i] - '0');
        } else {
            goto fail;
        }
    }
    return total;

fail:
    fprintf(stderr, "parse error at %d\n", total);
    return -2;
}

// compute a checksum
static unsigned crc32_small(const unsigned char *data, int n) {
    unsigned crc = 0xFFFFFFFFU;
    for (int i = 0; i < n; i++) {
        crc ^= data[i];
        for (int k = 0; k < 8; k++) {
            if (crc & 1) {
                crc = (crc >> 1) ^ 0xEDB88320U;
            } else {
                crc >>= 1;
            }
        }
    }
    return ~crc;
}

int dispatch(int op, int x) {
    switch (op) {
        case 1: return x + 1;
        case 2: return x * 2;
        case 3: return x - 1;
        default: return 0;
    }
}
