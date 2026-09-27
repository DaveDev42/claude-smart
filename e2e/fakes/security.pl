#!/usr/bin/perl
# e2e/fakes/security.pl -- a fake /usr/bin/security for the e2e harness.
#
# Run as `/usr/bin/perl security.pl <root> <security args...>` (csm's e2e
# build does this through CSM_E2E_SECURITY / CSM_E2E_SECURITY_ROOT). Items
# live as files, <root>/items/<hex(service)>.<hex(account)>, holding the
# exact bytes. Supports find-generic-password -s -a [-w],
# add-generic-password [-U] -a -s (-X <hex> | -w <value>),
# delete-generic-password -s -a, and -i (commands on stdin, one per line).
# The first word of every call is appended to <root>/calls; no value is
# ever logged. Kept in step with the unit tests' copy in
# src/orca/testsupport.rs.
use strict;
use warnings;
my $root = shift @ARGV;
sub hexs { return unpack('H*', $_[0]); }
sub item { return "$root/items/" . hexs($_[0]) . '.' . hexs($_[1]); }
sub opts {
    my ($valued, @a) = @_;
    my %o;
    while (@a) {
        my $k = shift @a;
        if ($valued->{$k}) { $o{$k} = shift @a; } else { $o{$k} = 1; }
    }
    return \%o;
}
sub notfound {
    print STDERR "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n";
    return 44;
}
sub run {
    my ($cmd, @a) = @_;
    $cmd = '' unless defined $cmd;
    if ($cmd eq 'find-generic-password') {
        my $o = opts({'-s' => 1, '-a' => 1}, @a);
        if (-e "$root/fail-find-" . hexs($o->{'-s'})) {
            print STDERR "security: fake access failure\n";
            return 1;
        }
        my $p = item($o->{'-s'}, $o->{'-a'});
        return notfound() unless -e $p;
        open(my $f, '<:raw', $p) or return 1;
        local $/;
        my $v = <$f>;
        close $f;
        print $v, "\n";
        return 0;
    }
    if ($cmd eq 'add-generic-password') {
        my $o = opts({'-s' => 1, '-a' => 1, '-X' => 1, '-w' => 1}, @a);
        if (-e "$root/fail-add-" . hexs($o->{'-s'})) {
            print STDERR "security: fake write failure\n";
            return 1;
        }
        return 0 if -e "$root/drop-add-" . hexs($o->{'-s'});
        my $p = item($o->{'-s'}, $o->{'-a'});
        if (-e $p && !$o->{'-U'}) {
            print STDERR "security: SecKeychainItemCreateFromContent: The specified item already exists in the keychain.\n";
            return 45;
        }
        my $v = exists $o->{'-X'} ? pack('H*', $o->{'-X'}) : $o->{'-w'};
        open(my $f, '>:raw', $p) or return 1;
        print $f $v;
        close $f;
        return 0;
    }
    if ($cmd eq 'delete-generic-password') {
        my $o = opts({'-s' => 1, '-a' => 1}, @a);
        my $p = item($o->{'-s'}, $o->{'-a'});
        return notfound() unless -e $p;
        unlink $p;
        return 0;
    }
    print STDERR "fake security: unsupported command\n";
    return 2;
}
open(my $log, '>>', "$root/calls") or die;
print $log (defined $ARGV[0] ? $ARGV[0] : ''), "\n";
close $log;
if (@ARGV && $ARGV[0] eq '-i') {
    my $rc = 0;
    while (my $line = <STDIN>) {
        chomp $line;
        next if $line =~ /^\s*$/;
        my @t;
        while ($line =~ /\s*(?:"((?:[^"\\]|\\.)*)"|(\S+))/g) {
            push @t, defined $1 ? $1 : $2;
        }
        $rc = run(@t);
    }
    exit $rc;
}
exit run(@ARGV);
