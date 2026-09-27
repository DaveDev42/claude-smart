#!/usr/bin/perl
# e2e/fakes/world.pl -- build and inspect the sandbox's Orca/Claude state.
#
#   perl -I e2e/fakes e2e/fakes/world.pl <command> [args]
#
# Commands (ids and grants are the fixtures below; tokens are fake):
#   reset                  Orca store with alice (active) and bob, both
#                          stashes, D materialized as alice, no Keychain
#                          leftovers
#   active                 the store's host active id
#   ids                    every account id in the store, one per line
#   stash-refresh <id>     the refresh token of <id>'s stashed grant
#   d-refresh              the refresh token of D's grant
#   d-uuid                 D's oauthAccount.accountUuid
#   rotate-d <access> <refresh>
#                          give D a newer grant (what Claude Code leaves after
#                          refreshing), keeping D's identity
#   legacy <name> <email> <uuid> <access> <refresh> [floor]
#                          a legacy ~/.claude.<name> profile in
#                          ~/.config/claude-as/profiles.json
#   drop <id>              remove <id> from the store and its stash
#   login-dir <dir> <email> <uuid> <access> <refresh>
#                          a config dir holding a Claude Code login (for
#                          `csm accounts import`), outside any registry
#   legacy-reset           a machine on the legacy layout: Orca's store
#                          with no account and no active id, no D, no
#                          ~/.claude.json, no Keychain items
#   shared <sid>           ~/.claude.shared with a transcript <sid>.jsonl
#                          (projects/-tmp-e2e-cwd), history.jsonl and todos,
#                          linked from every registered profile dir
#   set-active <id|null>   the store's active id
#   id-of <email>          the id of the account with <email>
#   rotate-dir <dir> <access> <refresh>
#                          give a config dir a newer grant (file and, on
#                          macOS, the dir's Keychain item)
#   marker <phase|boot|cutover>
#                          csm's migration.json: the phase, the cutover's
#                          boot id, or whether the cutover is recorded
#                          (none when the marker is absent)
#   sqlite                 mark the Orca profile SQLite-backed
#                          (profile-state.db beside orca-data.json)
# See World.pm for the environment it needs.

use strict;
use warnings;
use FindBin;
use lib $FindBin::Bin;
use World;
use File::Path qw(make_path remove_tree);

our %A = (id => 'aaaaaaaa-0000-4000-8000-00000000000a', email => 'alice@example.com', uuid => 'uuid-alice',
          access => 'at-alice-1', refresh => 'rt-alice-1');
our %B = (id => 'bbbbbbbb-0000-4000-8000-00000000000b', email => 'bob@example.com', uuid => 'uuid-bob',
          access => 'at-bob-1', refresh => 'rt-bob-1');
our $ORG = 'org-acme';
our $FAR = 4102444800000;    # 2100-01-01, never due for a refresh

# A config dir holding a login: the grant (file, plus the dir-scoped Keychain
# item on macOS, which is where Claude Code keeps it there) and the identity.
sub login_dir {
    my ($dir, $tag, $email, $uuid, $access, $refresh) = @_;
    make_path($dir);
    my $creds = World::creds_json($access, $refresh, $FAR);
    World::spew("$dir/.credentials.json", $creds);
    World::kc_put(World::runtime_service($dir), World::user(), $creds) if World::is_mac();
    World::write_json("$dir/.claude.json", {
        oauthAccount => World::oauth_json($uuid, $email, $ORG),
        hasCompletedOnboarding => JSON::PP::true,
        mcpServers => { "docs-$tag" => { command => "docs-server", args => [] } },
    });
}

my ($cmd, @args) = @ARGV;
$cmd //= '';

if ($cmd eq 'reset') {
    remove_tree(World::ud(), World::d_dir());
    unlink(World::home() . '/.claude.json');
    if (World::is_mac()) {
        my $items = "$ENV{E2E_SEC_ROOT}/items";
        remove_tree($items);
        make_path($items);
        unlink("$ENV{E2E_SEC_ROOT}/calls");
    }
    make_path(World::ud());
    World::write_index();
    my $ts = 1_700_000_000_000;
    World::save_store({
        schemaVersion => 1,
        settings => {
            claudeManagedAccounts => [
                World::record($A{id}, $A{email}, $ORG, $ts),
                World::record($B{id}, $B{email}, $ORG, $ts + 1),
            ],
            activeClaudeManagedAccountId => $A{id},
            activeClaudeManagedAccountIdsByRuntime => { host => $A{id}, wsl => {} },
        },
    });
    for my $x (\%A, \%B) {
        World::stash_write($x->{id}, World::creds_json($x->{access}, $x->{refresh}, $FAR),
            World::oauth_json($x->{uuid}, $x->{email}, $ORG));
    }
    World::write_json(World::home() . '/.claude.json', { numStartups => 3 });
    World::materialize(World::creds_json($A{access}, $A{refresh}, $FAR),
        World::oauth_json($A{uuid}, $A{email}, $ORG));
    exit 0;
}
if ($cmd eq 'active') { print((World::host_active(World::load_store()) // 'null'), "\n"); exit 0; }
if ($cmd eq 'ids') { print "$_->{id}\n" for @{ World::accounts(World::load_store()) }; exit 0; }
if ($cmd eq 'stash-refresh') { print World::refresh_of(World::stash_creds($args[0])), "\n"; exit 0; }
if ($cmd eq 'd-refresh') { print World::refresh_of(World::d_creds()), "\n"; exit 0; }
if ($cmd eq 'd-uuid') {
    my $cfg = World::read_json(World::config_path()) // {};
    print(($cfg->{oauthAccount}{accountUuid} // ''), "\n");
    exit 0;
}
if ($cmd eq 'rotate-d') {
    my ($access, $refresh) = @args;
    my $creds = World::creds_json($access, $refresh, $FAR + 1000);
    World::spew(World::d_dir() . '/.credentials.json', $creds);
    if (World::is_mac()) {
        World::kc_put(World::runtime_service(World::d_dir()), World::user(), $creds);
        World::kc_put($World::RUNTIME_SERVICE, World::user(), $creds);
    }
    exit 0;
}
if ($cmd eq 'legacy') {
    my ($name, $email, $uuid, $access, $refresh, $floor) = @args;
    my $dir = World::home() . "/.claude.$name";
    login_dir($dir, $name, $email, $uuid, $access, $refresh);
    my $reg = World::home() . '/.config/claude-as';
    my $map = World::read_json("$reg/profiles.json") // {};
    $map->{$name} = $dir;
    World::write_json("$reg/profiles.json", $map);
    World::spew("$reg/default", "$name\n") if $floor;
    exit 0;
}
if ($cmd eq "login-dir") {
    my ($dir, $email, $uuid, $access, $refresh) = @args;
    die "world.pl: login-dir must be inside the sandbox home\n" unless index($dir, World::home() . "/") == 0;
    login_dir($dir, "extra", $email, $uuid, $access, $refresh);
    exit 0;
}
if ($cmd eq 'legacy-reset') {
    remove_tree(World::ud(), World::d_dir());
    unlink(World::home() . '/.claude.json');
    if (World::is_mac()) {
        my $items = "$ENV{E2E_SEC_ROOT}/items";
        remove_tree($items);
        make_path($items);
        unlink("$ENV{E2E_SEC_ROOT}/calls");
    }
    make_path(World::ud());
    World::write_index();
    World::save_store({
        schemaVersion => 1,
        settings => {
            claudeManagedAccounts => [],
            activeClaudeManagedAccountId => undef,
            activeClaudeManagedAccountIdsByRuntime => { host => undef, wsl => {} },
        },
    });
    exit 0;
}
if ($cmd eq 'shared') {
    my ($sid) = @args;
    my $sh = World::home() . '/.claude.shared';
    make_path("$sh/projects/-tmp-e2e-cwd", "$sh/todos");
    World::spew("$sh/projects/-tmp-e2e-cwd/$sid.jsonl", qq({"type":"user","sessionId":"$sid"}\n), 0644);
    World::spew("$sh/history.jsonl", qq({"display":"hello from the shared history"}\n), 0644);
    # Older than csm's 5-minute "being written" window.
    my $old = time() - 3600;
    utime($old, $old, "$sh/history.jsonl", "$sh/projects/-tmp-e2e-cwd/$sid.jsonl");
    my $map = World::read_json(World::home() . '/.config/claude-as/profiles.json') // {};
    for my $dir (values %$map) {
        for my $n (qw(projects history.jsonl todos)) {
            unlink "$dir/$n";
            symlink("$sh/$n", "$dir/$n") or die "world.pl: cannot link $dir/$n: $!\n";
        }
    }
    exit 0;
}
if ($cmd eq 'set-active') {
    my $s = World::load_store();
    World::set_active($s, $args[0] eq 'null' ? undef : $args[0]);
    World::save_store($s);
    exit 0;
}
if ($cmd eq 'id-of') {
    my ($r) = grep { lc($_->{email} // '') eq lc($args[0]) } @{ World::accounts(World::load_store()) };
    print(($r ? $r->{id} : ''), "\n");
    exit 0;
}
if ($cmd eq 'rotate-dir') {
    my ($dir, $access, $refresh) = @args;
    die "world.pl: rotate-dir must be inside the sandbox home\n" unless index($dir, World::home() . "/") == 0;
    my $creds = World::creds_json($access, $refresh, $FAR + 1000);
    World::spew("$dir/.credentials.json", $creds);
    World::kc_put(World::runtime_service($dir), World::user(), $creds) if World::is_mac();
    exit 0;
}
if ($cmd eq 'marker') {
    my $m = World::read_json(World::home() . '/.local/state/csm/migration.json');
    if (!$m) { print "none\n"; exit 0; }
    if ($args[0] eq 'phase') { print(($m->{phase} // ''), "\n"); }
    elsif ($args[0] eq 'boot') { print((($m->{cutover} // {})->{boot_id} // ''), "\n"); }
    elsif ($args[0] eq 'cutover') { print(($m->{cutover} ? 'yes' : 'no'), "\n"); }
    else { die "world.pl: marker phase|boot|cutover\n"; }
    exit 0;
}
if ($cmd eq 'sqlite') {
    my ($dir) = World::store_path() =~ m{^(.*)/[^/]+$};
    World::spew("$dir/profile-state.db", "SQLite format 3\0", 0600);
    exit 0;
}
if ($cmd eq 'drop') {
    my $s = World::load_store();
    $s->{settings}{claudeManagedAccounts} = [grep { $_->{id} ne $args[0] } @{ World::accounts($s) }];
    World::save_store($s);
    World::stash_remove($args[0]);
    exit 0;
}
die "world.pl: unknown command '$cmd'\n";
