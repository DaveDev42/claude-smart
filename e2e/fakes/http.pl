#!/usr/bin/perl
# e2e/fakes/http.pl -- a loopback stand-in for Anthropic's OAuth endpoints.
#
#   perl http.pl <rules-dir>
#
# Listens on 127.0.0.1 (a free port, written to <rules-dir>/port) and
# answers from files the scenario writes:
#   GET  /api/oauth/profile   Authorization: Bearer <t>  -> profile/<t>
#   POST /v1/oauth/token      refresh_token=<r>          -> token/<r>
#   GET  /api/oauth/usage     Authorization: Bearer <t>  -> usage/<t>
# A rule file's first line is the status, the rest the body. No rule means
# 599, so an unexpected call shows up as a failure. Every request appends
# "<method> <path> <rule-name>" to <rules-dir>/requests.log. The tokens are
# the harness's fakes; nothing else ever reaches this server.

use strict;
use warnings;
use IO::Socket::INET;

my $dir = shift @ARGV or die "usage: http.pl <rules-dir>\n";
my $srv = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => 0, Listen => 16, ReuseAddr => 1, Proto => 'tcp')
    or die "http.pl: cannot listen: $!\n";
my $done = 0;
$SIG{TERM} = $SIG{INT} = $SIG{HUP} = sub { $done = 1; };
{
    open(my $f, '>', "$dir/port.tmp") or die;
    print $f $srv->sockport, "\n";
    close $f;
    rename("$dir/port.tmp", "$dir/port");
}

sub rule {
    my ($kind, $name) = @_;
    return (599, '{"error":"no rule"}') unless defined $name && $name =~ /^[A-Za-z0-9._-]+$/;
    open(my $f, '<', "$dir/$kind/$name") or return (599, '{"error":"no rule"}');
    my $status = <$f> // '599';
    $status =~ s/\s+\z//;
    local $/;
    my $body = <$f> // '';
    close $f;
    return ($status, $body);
}

sub logreq {
    open(my $f, '>>', "$dir/requests.log") or return;
    print $f "@_\n";
    close $f;
}

use IO::Select;
my $sel = IO::Select->new($srv);
while (!$done) {
    next unless $sel->can_read(0.2);
    my $c = $srv->accept or next;
    $c->timeout(5);
    my $line = <$c>;
    next unless defined $line;
    my ($method, $path) = $line =~ /^(\S+)\s+(\S+)/;
    my %h;
    while (my $l = <$c>) {
        $l =~ s/\r?\n$//;
        last if $l eq '';
        my ($k, $v) = split(/:\s*/, $l, 2);
        $h{ lc $k } = $v;
    }
    my $body = '';
    read($c, $body, $h{'content-length'}) if $h{'content-length'};
    my ($bearer) = ($h{authorization} // '') =~ /^Bearer\s+(\S+)/;
    $path //= '';
    $path =~ s/\?.*//;
    my ($status, $out, $name);
    if ($path eq '/api/oauth/profile') {
        $name = $bearer;
        ($status, $out) = rule('profile', $name);
    } elsif ($path eq '/v1/oauth/token') {
        ($name) = $body =~ /(?:^|&)refresh_token=([^&]*)/;
        ($status, $out) = rule('token', $name);
    } elsif ($path eq '/api/oauth/usage') {
        $name = $bearer;
        ($status, $out) = rule('usage', $name);
    } else {
        ($status, $out) = (404, '{}');
    }
    logreq($method // '?', $path, $name // '-');
    print $c "HTTP/1.1 $status X\r\nContent-Type: application/json\r\nContent-Length: " . length($out)
        . "\r\nConnection: close\r\n\r\n$out";
    close $c;
}
unlink "$dir/port";
exit 0;
