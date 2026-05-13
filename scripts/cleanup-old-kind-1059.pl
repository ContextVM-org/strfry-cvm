#!/usr/bin/env perl

use strict;
use warnings;
use JSON::PP qw(encode_json);
use Getopt::Long qw(GetOptions);

$| = 1;

my $strfry_bin = '/usr/local/bin/strfry';
my $max_age_seconds = 24 * 60 * 60;
my $kind = 1059;
my $dry_run = 0;

GetOptions(
    'strfry-bin=s'     => \$strfry_bin,
    'max-age-seconds=i' => \$max_age_seconds,
    'kind=i'           => \$kind,
    'dry-run'          => \$dry_run,
) or die "usage: $0 [--strfry-bin PATH] [--max-age-seconds N] [--kind N] [--dry-run]\n";

die "--max-age-seconds must be greater than zero\n" if $max_age_seconds <= 0;
die "--kind must be greater than zero\n" if $kind <= 0;

my $until = time() - $max_age_seconds;
my $filter = encode_json({ kinds => [$kind], until => $until });

if ($dry_run) {
    print "dry run: would delete events with filter $filter\n";
    exit 0;
}

my @cmd = ($strfry_bin, 'delete', '--filter', $filter);
system(@cmd) == 0 or die "failed to execute @cmd: $?\n";

print "deleted old events with filter $filter\n";
