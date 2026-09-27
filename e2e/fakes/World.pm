package World;
# e2e/fakes/World.pm -- the sandbox's Orca and Claude Code state, shared by
# the fake Orca (orca.pl) and the harness's fixture helper (world.pl).
#
# Everything lives under the sandbox named by the environment:
#   E2E_HOME      the sandbox HOME (D is $E2E_HOME/.claude)
#   E2E_UD        Orca's userData inside it
#   E2E_SEC_ROOT  the fake Keychain's root (macOS only; see security.pl)
#   USER          the Keychain account name Claude Code and Orca use
# It refuses to start when E2E_HOME or E2E_UD is unset or when E2E_UD is not
# under E2E_HOME, so it can never touch a real home.
#
# The layouts follow Orca 1.4.214: the store is
# <userData>/profiles/<profile>/orca-data.json chosen through
# orca-profile-index.json, stashes are <userData>/claude-accounts/<id>/auth
# with the ".orca-managed-claude-auth" marker, and the stashed grant is a
# Keychain item (service "Orca Claude Code Managed Credentials", account
# <id>) on macOS and <auth>/.credentials.json elsewhere.

use strict;
use warnings;
use JSON::PP;
use Digest::SHA qw(sha256_hex);
use File::Path qw(make_path remove_tree);
use Time::HiRes qw(time);

our $PROFILE_ID = 'p-e2e';
our $STASH_SERVICE = 'Orca Claude Code Managed Credentials';
our $RUNTIME_SERVICE = 'Claude Code-credentials';

my $JSON = JSON::PP->new->utf8->canonical;

sub home { return $ENV{E2E_HOME} // die "E2E_HOME unset\n"; }
sub ud { return $ENV{E2E_UD} // die "E2E_UD unset\n"; }

BEGIN {
    my ($h, $u) = ($ENV{E2E_HOME}, $ENV{E2E_UD});
    die "World.pm: E2E_HOME and E2E_UD must be set\n" unless $h && $u;
    die "World.pm: E2E_UD must be inside E2E_HOME\n" unless index($u, "$h/") == 0;
}

sub is_mac { return $^O eq 'darwin'; }
sub now_ms { return int(time() * 1000); }
sub user { my $u = $ENV{USER}; return (defined $u && $u =~ /^[A-Za-z0-9._-]+$/) ? $u : 'claude-code-user'; }

# ─── files ─────────────────────────────────────────────────────────────────────

sub slurp {
    my ($p) = @_;
    open(my $f, '<:raw', $p) or return undef;
    local $/;
    my $v = <$f>;
    close $f;
    return $v;
}

sub spew {
    my ($p, $bytes, $mode) = @_;
    my ($dir) = $p =~ m{^(.*)/[^/]+$};
    make_path($dir) if defined $dir && !-d $dir;
    my $tmp = "$p.tmp.$$";
    open(my $f, '>:raw', $tmp) or die "cannot write $tmp: $!\n";
    print $f $bytes;
    close $f;
    chmod($mode // 0600, $tmp);
    rename($tmp, $p) or die "cannot rename $tmp: $!\n";
}

sub read_json {
    my ($p) = @_;
    my $t = slurp($p);
    return undef unless defined $t;
    my $v = eval { $JSON->decode($t) };
    return $v;
}

sub write_json { my ($p, $v) = @_; spew($p, $JSON->encode($v)); }
sub encode { return $JSON->encode($_[0]); }
sub decode { return $JSON->decode($_[0]); }

# ─── fake Keychain (macOS) ─────────────────────────────────────────────────────

sub kc_item {
    my ($svc, $acct) = @_;
    my $root = $ENV{E2E_SEC_ROOT} // die "E2E_SEC_ROOT unset\n";
    return "$root/items/" . unpack('H*', $svc) . '.' . unpack('H*', $acct);
}
sub kc_get { return slurp(kc_item(@_)); }
sub kc_put { my ($svc, $acct, $v) = @_; spew(kc_item($svc, $acct), $v); }
sub kc_del { unlink kc_item(@_); }

# Claude Code's service for a config dir: the unscoped name, or the name
# suffixed with the first 8 hex chars of sha256(dir).
sub runtime_service {
    my ($dir) = @_;
    return $RUNTIME_SERVICE unless defined $dir && length $dir;
    return "$RUNTIME_SERVICE-" . substr(sha256_hex($dir), 0, 8);
}

# ─── store ─────────────────────────────────────────────────────────────────────

sub store_path { return ud() . "/profiles/$PROFILE_ID/orca-data.json"; }

sub write_index {
    my $p = {
        id => $PROFILE_ID, name => 'E2E', kind => 'local',
        createdAt => 1, updatedAt => 1, lastOpenedAt => 1,
        avatar => { kind => 'initials', initials => 'E', color => 'neutral' },
    };
    write_json(ud() . '/orca-profile-index.json', { activeProfileId => $PROFILE_ID, profiles => [$p] });
}

sub load_store { return read_json(store_path()) // die "no store at " . store_path() . "\n"; }
sub save_store { write_json(store_path(), $_[0]); }

# Orca resolves the stash root per call after app.setName('Orca'), so it
# lives under the late userData <appData>/Orca. On macOS's case-insensitive
# filesystem that is the canonical dir; on Linux it is a separate dir.
sub late_ud {
    my $u = ud();
    return $u if $^O eq 'darwin' || $u !~ m{/orca\z};
    (my $late = $u) =~ s{/orca\z}{/Orca};
    return $late;
}
sub stash_dir { return late_ud() . "/claude-accounts/$_[0]/auth"; }

sub record {
    my ($id, $email, $org, $ts) = @_;
    return {
        id => $id, email => $email, managedAuthPath => stash_dir($id),
        managedAuthRuntime => 'host', wslDistro => undef, wslLinuxAuthPath => undef,
        authMethod => 'subscription-oauth', organizationUuid => $org, organizationName => undef,
        createdAt => $ts, updatedAt => $ts, lastAuthenticatedAt => $ts,
    };
}

sub accounts { return $_[0]{settings}{claudeManagedAccounts} // []; }
sub host_active { return $_[0]{settings}{activeClaudeManagedAccountIdsByRuntime}{host}; }

sub set_active {
    my ($store, $id) = @_;
    $store->{settings}{activeClaudeManagedAccountId} = $id;
    $store->{settings}{activeClaudeManagedAccountIdsByRuntime} //= { wsl => {} };
    $store->{settings}{activeClaudeManagedAccountIdsByRuntime}{host} = $id;
}

# Orca's list view: records without managedAuthPath, newest first.
sub snapshot {
    my ($store) = @_;
    my @view;
    for my $r (@{ accounts($store) }) {
        my %c = %$r;
        delete $c{managedAuthPath};
        push @view, \%c;
    }
    @view = sort { ($b->{updatedAt} // 0) <=> ($a->{updatedAt} // 0) } @view;
    my $by = $store->{settings}{activeClaudeManagedAccountIdsByRuntime} // {};
    return {
        accounts => \@view,
        activeAccountId => $store->{settings}{activeClaudeManagedAccountId},
        activeAccountIdsByRuntime => { host => $by->{host}, wsl => ($by->{wsl} // {}) },
    };
}

# ─── stashes ───────────────────────────────────────────────────────────────────

sub stash_write {
    my ($id, $creds, $oauth) = @_;
    my $d = stash_dir($id);
    make_path($d);
    spew("$d/.orca-managed-claude-auth", "$id\n");
    spew("$d/oauth-account.json", encode($oauth)) if $oauth;
    if (is_mac()) { kc_put($STASH_SERVICE, $id, $creds); }
    else { spew("$d/.credentials.json", $creds); }
}

sub stash_creds {
    my ($id) = @_;
    return is_mac() ? kc_get($STASH_SERVICE, $id) : slurp(stash_dir($id) . '/.credentials.json');
}

sub stash_oauth { return read_json(stash_dir($_[0]) . '/oauth-account.json'); }

sub stash_remove {
    my ($id) = @_;
    remove_tree(late_ud() . "/claude-accounts/$id");
    kc_del($STASH_SERVICE, $id) if is_mac();
}

# ─── D (~/.claude) ─────────────────────────────────────────────────────────────

# D: $E2E_ORCA_D when the fake Orca runs with CLAUDE_CONFIG_DIR set (the
# legacy floor, see start-orca.sh), else ~/.claude. With the variable set
# Orca and Claude Code read <D>/.claude.json; without it ~/.claude.json,
# unless ~/.claude/.claude.json exists (Orca's resolveConfigPath).
sub d_explicit {
    my $d = $ENV{E2E_ORCA_D};
    return undef unless defined $d && length $d;
    die "World.pm: E2E_ORCA_D must be inside E2E_HOME\n" unless index($d, home() . '/') == 0;
    return $d;
}
sub d_dir { return d_explicit() // home() . '/.claude'; }
sub config_path {
    return d_explicit() . '/.claude.json' if defined d_explicit();
    my $in = d_dir() . '/.claude.json';
    return -e $in ? $in : home() . '/.claude.json';
}

sub set_oauth_account {
    my ($oauth) = @_;
    my $p = config_path();
    my $cfg = read_json($p) // {};
    if ($oauth) { $cfg->{oauthAccount} = $oauth; } else { delete $cfg->{oauthAccount}; }
    write_json($p, $cfg);
}

# Orca's materialize: the file, then (macOS) the scoped and unscoped items,
# then the identity.
sub materialize {
    my ($creds, $oauth) = @_;
    make_path(d_dir());
    spew(d_dir() . '/.credentials.json', $creds);
    if (is_mac()) {
        kc_put(runtime_service(d_dir()), user(), $creds);
        kc_put($RUNTIME_SERVICE, user(), $creds);
    }
    set_oauth_account($oauth);
}

sub d_creds { return slurp(d_dir() . '/.credentials.json'); }

sub refresh_of {
    my ($creds) = @_;
    return '' unless defined $creds;
    my $v = eval { decode($creds) } or return '';
    return $v->{claudeAiOauth}{refreshToken} // '';
}

# ─── grants ────────────────────────────────────────────────────────────────────

sub creds_json {
    my ($access, $refresh, $expires) = @_;
    return encode({ claudeAiOauth => {
        accessToken => $access, refreshToken => $refresh, expiresAt => 0 + $expires,
        scopes => ['user:inference'], subscriptionType => 'max',
    } });
}

sub oauth_json {
    my ($uuid, $email, $org) = @_;
    return { accountUuid => $uuid, emailAddress => $email, organizationUuid => $org, organizationName => 'Acme' };
}

sub new_id {
    my @h = map { sprintf('%02x', int(rand(256))) } 1 .. 16;
    $h[6] = sprintf('%02x', (hex($h[6]) & 0x0f) | 0x40);
    $h[8] = sprintf('%02x', (hex($h[8]) & 0x3f) | 0x80);
    my $s = join('', @h);
    return join('-', substr($s, 0, 8), substr($s, 8, 4), substr($s, 12, 4), substr($s, 16, 4), substr($s, 20, 12));
}

1;
