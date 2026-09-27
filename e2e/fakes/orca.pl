#!/usr/bin/perl
# e2e/fakes/orca.pl -- a fake Orca runtime for the e2e harness.
#
#   perl -I e2e/fakes orca.pl serve <main-pid> <socket> <request-log>
#   perl -I e2e/fakes orca.pl call <method> <params-json>
#
# `serve` plays Orca 1.4.214's local RPC runtime: it writes
# <userData>/orca-runtime.json naming <main-pid> (the fake main process that
# holds SingletonLock) and a unix socket, loads the store once at start the
# way Orca does, and answers newline-delimited JSON requests
# {id, authToken, method, params} with {id, ok, result|error, _meta}. It
# implements the four methods csm calls:
#   accounts.list                    the claude snapshot plus empty usage
#   accounts.selectClaude            set the host active id, materialize D
#                                    from the stash, persist
#   accounts.removeClaude            drop the record and its stash
#   accounts.addClaudeFromConfigDir  capture a dir's login into a stash
# Each request is logged as "<caller> <method> <params>" (caller "gui" for
# `call`, else "csm"); the auth token is never logged. SIGTERM removes the
# runtime file and the socket.
#
# `call` is the Orca GUI: it sends one request with the runtime file's token
# and prints the response.

use strict;
use warnings;
use FindBin;
use lib $FindBin::Bin;
use World;
use IO::Socket::UNIX;
use IO::Select;
use Socket qw(SOCK_STREAM);

my $mode = shift @ARGV // '';
my $runtime_file = World::ud() . '/orca-runtime.json';

if ($mode eq 'call') {
    my ($method, $params) = @ARGV;
    my $meta = World::read_json($runtime_file) or die "orca.pl call: no runtime file\n";
    my $ep = $meta->{transports}[0]{endpoint};
    my $s = IO::Socket::UNIX->new(Type => SOCK_STREAM, Peer => $ep) or die "orca.pl call: connect: $!\n";
    my $req = { id => 'gui-' . World::new_id(), authToken => $meta->{authToken}, method => $method,
                params => World::decode($params // '{}') };
    print $s World::encode($req), "\n";
    my $line = <$s>;
    print $line // '';
    exit(defined $line && $line =~ /"ok":true/ ? 0 : 1);
}

die "usage: orca.pl serve <main-pid> <socket> <log> | call <method> <params>\n" unless $mode eq 'serve';
my ($main_pid, $sock_path, $log_path) = @ARGV;

my $rid = 'rt-e2e-' . World::new_id();
my $token = 'e2e-token-' . World::new_id();
unlink $sock_path;
my $srv = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => $sock_path, Listen => 16)
    or die "orca.pl: cannot listen on $sock_path: $!\n";

my $done = 0;
$SIG{TERM} = $SIG{INT} = $SIG{HUP} = sub { $done = 1; };

my $store = World::load_store();
World::spew($runtime_file, World::encode({
    runtimeId => $rid, pid => 0 + $main_pid,
    transports => [{ kind => 'unix', endpoint => $sock_path }],
    authToken => $token, startedAt => World::now_ms(),
}));

sub logline {
    open(my $f, '>>', $log_path) or return;
    print $f "@_\n";
    close $f;
}

sub fail { return { error => { code => $_[0], message => $_[1] } }; }

sub find_idx {
    my ($id) = @_;
    my $a = World::accounts($store);
    for my $i (0 .. $#$a) { return $i if $a->[$i]{id} eq $id; }
    return -1;
}

sub select_claude {
    my ($id) = @_;
    return fail('not_found', 'no such account') if !defined $id || find_idx($id) < 0;
    my $creds = World::stash_creds($id);
    return fail('stash', 'stashed credentials missing') unless defined $creds;
    World::materialize($creds, World::stash_oauth($id));
    World::set_active($store, $id);
    World::save_store($store);
    return { result => World::snapshot($store) };
}

sub remove_claude {
    my ($id) = @_;
    my $i = find_idx($id // '');
    return fail('not_found', 'no such account') if $i < 0;
    splice(@{ $store->{settings}{claudeManagedAccounts} }, $i, 1);
    World::stash_remove($id);
    World::set_active($store, undef) if (World::host_active($store) // '') eq $id;
    World::save_store($store);
    return { result => World::snapshot($store) };
}

sub add_from_dir {
    my ($dir) = @_;
    return fail('invalid', 'configDir required') unless defined $dir && length $dir;
    my $creds = World::is_mac() ? World::kc_get(World::runtime_service($dir), World::user()) : undef;
    $creds //= World::slurp("$dir/.credentials.json");
    return fail('no_credentials', 'No Claude credentials found') unless defined $creds;
    my $cfg = World::read_json("$dir/.claude.json") // {};
    my $oauth = $cfg->{oauthAccount} // {};
    my $email = lc($oauth->{emailAddress} // '');
    return fail('no_email', 'could not resolve the account email') unless length $email;
    my $org = $oauth->{organizationUuid};
    my $now = World::now_ms();
    my ($rec) = grep {
        lc($_->{email} // '') eq $email && (($_->{organizationUuid} // '') eq ($org // ''))
    } @{ World::accounts($store) };
    if ($rec) {
        $rec->{updatedAt} = $now;
        $rec->{lastAuthenticatedAt} = $now;
    } else {
        $rec = World::record(World::new_id(), $oauth->{emailAddress}, $org, $now);
        push @{ $store->{settings}{claudeManagedAccounts} }, $rec;
    }
    World::stash_write($rec->{id}, $creds, $oauth);
    World::save_store($store);
    return { result => World::snapshot($store) };
}

sub handle {
    my ($req) = @_;
    return fail('unauthorized', 'bad token') unless ($req->{authToken} // '') eq $token;
    my $m = $req->{method} // '';
    my $p = $req->{params} // {};
    my $caller = ($req->{id} // '') =~ /^gui-/ ? 'gui' : 'csm';
    my %shown = %$p;
    logline($caller, $m, World::encode(\%shown));
    if ($m eq 'accounts.list') {
        return { result => {
            claude => World::snapshot($store),
            codex => { accounts => [], activeAccountId => undef },
            rateLimits => { claude => undef, codex => undef, inactiveClaudeAccounts => [] },
        } };
    }
    return select_claude($p->{accountId}) if $m eq 'accounts.selectClaude';
    return remove_claude($p->{accountId}) if $m eq 'accounts.removeClaude';
    return add_from_dir($p->{configDir}) if $m eq 'accounts.addClaudeFromConfigDir';
    return fail('method_not_found', "unknown method $m");
}

sub respond {
    my ($c, $line) = @_;
    my $req = eval { World::decode($line) };
    my $out = $req ? eval { handle($req) } // fail('internal', "$@") : fail('parse', 'bad request');
    my $frame = { id => ($req ? $req->{id} : undef), _meta => { runtimeId => $rid } };
    if (exists $out->{error}) { $frame->{ok} = JSON::PP::false; $frame->{error} = $out->{error}; }
    else { $frame->{ok} = JSON::PP::true; $frame->{result} = $out->{result}; }
    print $c World::encode($frame), "\n";
}

my $sel = IO::Select->new($srv);
while (!$done) {
    next unless $sel->can_read(0.2);
    my $c = $srv->accept or next;
    $c->autoflush(1);
    my $cs = IO::Select->new($c);
    my $buf = '';
    my $deadline = time + 10;
    while (!$done && time < $deadline) {
        next unless $cs->can_read(0.2);
        my $n = sysread($c, my $chunk, 65536);
        last unless $n;
        $buf .= $chunk;
        while ($buf =~ s/^([^\n]*)\n//) { respond($c, $1); }
    }
    close $c;
}
unlink $runtime_file, $sock_path;
exit 0;
