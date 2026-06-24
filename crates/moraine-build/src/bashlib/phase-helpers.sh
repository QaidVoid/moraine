# Moraine vendored bash phase library: helper-function surface.
#
# This is the helper contract ebuilds and eclasses call: econf, emake, unpack,
# use, the do*/new* install family, eapply, elog/einfo/ewarn/eerror/eqawarn, and
# the IPC-backed has_version/best_version. The Rust driver never reimplements any
# of these; it provides the environment and the IPC channel they rely on.
#
# This vendored copy is intentionally a thin, auditable subset that documents the
# surface contract. A production deployment replaces it with the full lightly
# patched fork of the stock bin/phase-helpers.sh.

die() {
    echo "die: $*" >&2
    exit 1
}

# --- elog family ------------------------------------------------------------
# Messages are tagged so the Rust log-and-elog capture can classify them by
# severity. The driver scans the build log for these tags.

__elog_emit() {
    local level=$1; shift
    echo "MORAINE_ELOG ${level} ${EBUILD_PHASE:-unknown} $*"
}

einfo()    { __elog_emit INFO "$@"; }
elog()     { __elog_emit LOG "$@"; }
ewarn()    { __elog_emit WARN "$@"; }
eerror()   { __elog_emit ERROR "$@"; }
eqawarn()  { __elog_emit QA "$@"; }

# --- USE helpers ------------------------------------------------------------

use() {
    local flag=${1#!}
    local negate=
    [[ ${1} == !* ]] && negate=1
    if [[ " ${USE} " == *" ${flag} "* ]]; then
        [[ -n ${negate} ]] && return 1 || return 0
    else
        [[ -n ${negate} ]] && return 0 || return 1
    fi
}

usev() { use "$1" && echo "${1#!}"; }

# --- build helpers ----------------------------------------------------------

econf() {
    local conf=${ECONF_SOURCE:-.}/configure
    [[ -x ${conf} ]] || die "no configure script"
    "${conf}" \
        --prefix="${EPREFIX}/usr" \
        --build="${CBUILD:-${CHOST}}" \
        --host="${CHOST}" \
        "$@" || die "econf failed"
}

emake() {
    make ${MAKEOPTS} "$@"
}

unpack() {
    local file
    for file in "$@"; do
        local src="${DISTDIR}/${file}"
        case ${file} in
            *.tar.gz|*.tgz)  tar xzf "${src}" || die "unpack failed: ${file}" ;;
            *.tar.bz2|*.tbz2) tar xjf "${src}" || die "unpack failed: ${file}" ;;
            *.tar.xz|*.txz)  tar xJf "${src}" || die "unpack failed: ${file}" ;;
            *.tar)           tar xf "${src}" || die "unpack failed: ${file}" ;;
            *.zip)           unzip -q "${src}" || die "unpack failed: ${file}" ;;
            *) die "unknown archive: ${file}" ;;
        esac
    done
}

# --- install family ---------------------------------------------------------
# The full vendored library implements the do*/new* family; the contract here is
# that they stage into ${D} under the install phase's faked privilege.

dodir()  { mkdir -p "${D}${EPREFIX}$1"; }
dobin()  { dodir /usr/bin; install -m0755 "$@" "${D}${EPREFIX}/usr/bin/"; }
doexe()  { install -m0755 "$@" "${D}${EPREFIX}${into:-/usr/bin}/"; }
doins()  { install -m0644 "$@" "${D}${EPREFIX}${insinto:-/usr/share}/"; }

# --- patch helpers ----------------------------------------------------------

eapply() {
    local patch
    for patch in "$@"; do
        [[ ${patch} == -* ]] && continue
        patch -p1 < "${patch}" || die "eapply failed: ${patch}"
    done
}

eapply_user() { :; }

# --- IPC-backed version queries ---------------------------------------------
# These call the moraine IPC helper, which writes a request to the FIFO under
# .ipc and reads the response. The Rust manager answers from the installed store
# and repository. The helper path is exported by the driver as MORAINE_IPC_HELPER.

has_version() {
    "${MORAINE_IPC_HELPER}" has_version "$@"
}

best_version() {
    "${MORAINE_IPC_HELPER}" best_version "$@"
}
