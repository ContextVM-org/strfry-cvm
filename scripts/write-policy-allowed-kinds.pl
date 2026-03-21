#!/usr/bin/env perl

use strict;
use warnings;
use JSON::PP qw(decode_json encode_json);

$| = 1;

sub is_allowed_kind {
    my ($kind) = @_;

    return 1 if $kind == 1059;
    return 1 if $kind == 21059;
    return 1 if $kind == 25910;
    return 1 if $kind >= 10000 && $kind <= 19999;

    return 0;
}

while (my $line = <STDIN>) {
    chomp $line;

    my $req = eval { decode_json($line) };
    if ($@) {
        print STDERR "failed to decode JSON input: $@\n";
        next;
    }

    if (($req->{type} // '') ne 'new') {
        print STDERR "unexpected request type\n";
        next;
    }

    my $event = $req->{event} // {};
    my $id = $event->{id};
    my $kind = $event->{kind};

    my $res = { id => $id };

    if (defined $kind && is_allowed_kind($kind)) {
        $res->{action} = 'accept';
    } else {
        $res->{action} = 'reject';
        $res->{msg} = defined $kind
            ? "blocked: event kind $kind is not allowed"
            : 'blocked: missing event kind';
    }

    print encode_json($res), "\n";
}
