# Sample R file for complexity analysis.

MAX_SIZE <- 256

parse_header <- function(buf) {
  if (length(buf) == 0) {
    return(-1)
  }
  total <- 0
  for (b in buf) {
    if (b == 10) {
      next
    }
    if (b >= 48 && b <= 57) {
      total <- total * 10 + (b - 48)
    } else {
      stop(sprintf("parse error at %d", total))
    }
  }
  total
}

dispatch <- function(op, x) {
  if (op == 1) {
    x + 1
  } else if (op == 2) {
    x * 2
  } else if (op == 3) {
    x - 1
  } else {
    0
  }
}

crc32_small <- function(data) {
  crc <- bitwNot(0L)
  for (d in data) {
    crc <- bitwXor(crc, d)
    for (k in 1:8) {
      if (bitwAnd(crc, 1L) != 0) {
        crc <- bitwXor(bitwShiftR(crc, 1L), 0xEDB88320L)
      } else {
        crc <- bitwShiftR(crc, 1L)
      }
    }
  }
  bitwNot(crc)
}
