! Sample Fortran file for complexity analysis.
module sample
  implicit none
  integer, parameter :: MAX_SIZE = 256
contains

  integer function parse_header(buf, n) result(total)
    character(len=*), intent(in) :: buf
    integer, intent(in)          :: n
    integer :: i, b
    total = 0
    if (n <= 0) then
      total = -1
      return
    end if
    do i = 1, n
      b = iachar(buf(i:i))
      if (b == 10) then
        cycle
      end if
      if (b >= 48 .and. b <= 57) then
        total = total * 10 + (b - 48)
      else
        total = -2
        return
      end if
    end do
  end function parse_header

  integer function dispatch(op, x) result(y)
    integer, intent(in) :: op, x
    if (op == 1) then
      y = x + 1
    else if (op == 2) then
      y = x * 2
    else if (op == 3) then
      y = x - 1
    else
      y = 0
    end if
  end function dispatch

  subroutine crc32_small(data, n, crc)
    integer, intent(in)  :: data(:), n
    integer, intent(out) :: crc
    integer :: i, k
    crc = -1  ! 0xFFFFFFFF
    do i = 1, n
      crc = ieor(crc, data(i))
      do k = 1, 8
        if (iand(crc, 1) /= 0) then
          crc = ieor(ishft(crc, -1), int(z'EDB88320'))
        else
          crc = ishft(crc, -1)
        end if
      end do
    end do
    crc = not(crc)
  end subroutine crc32_small

end module sample
