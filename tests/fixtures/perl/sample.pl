#!/usr/bin/perl
# Sample Perl file for complexity analysis.
use strict;
use warnings;

my $MAX_SIZE = 256;

sub parse_header {
    my ($buf) = @_;
    return -1 unless defined $buf && length $buf;

    my $total = 0;
    for my $c (split //, $buf) {
        my $b = ord $c;
        next if $b == 10;
        if ($b >= 48 && $b <= 57) {
            $total = $total * 10 + ($b - 48);
        } else {
            die sprintf("parse error at %d", $total);
        }
    }
    return $total;
}

sub dispatch {
    my ($op, $x) = @_;
    if ($op == 1) {
        return $x + 1;
    } elsif ($op == 2) {
        return $x * 2;
    } elsif ($op == 3) {
        return $x - 1;
    } else {
        return 0;
    }
}

sub crc32_small {
    my ($data) = @_;
    my $crc = 0xFFFFFFFF;
    for my $d (unpack 'C*', $data) {
        $crc ^= $d;
        for (1 .. 8) {
            if ($crc & 1) {
                $crc = ($crc >> 1) ^ 0xEDB88320;
            } else {
                $crc >>= 1;
            }
        }
    }
    return (~$crc) & 0xFFFFFFFF;
}

package Counter;
sub new {
    my ($class) = @_;
    bless { n => 0 }, $class;
}

sub inc {
    my ($self, $by) = @_;
    die "neg" if $by < 0;
    $self->{n} += $by;
    return $self->{n};
}

1;
