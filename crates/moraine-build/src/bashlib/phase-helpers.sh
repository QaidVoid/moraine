# Moraine vendored bash phase library: helper-function surface.
#
# Faithfully ported from the stock Portage bin/phase-helpers.sh and the
# bin/ebuild-helpers/* install family. This is the helper contract ebuilds and
# eclasses call: the USE helpers, econf, emake, unpack, eapply/eapply_user,
# einstalldocs, the do*/new* install family, the EAPI-versioned default phase
# implementations, and the IPC-backed has_version/best_version.
#
# The do*/new* family is implemented as bash functions here (rather than as
# bin/ebuild-helpers/* scripts on PATH) to keep the whole helper surface in the
# sourced library, matching moraine's per-phase driver boundary.

# Install-helper defaults and the doc/strip path lists, mirroring the exports at
# the top of bin/phase-helpers.sh. MOPREFIX seeds the message-catalog name so an
# eclass or ebuild override is honored by domo. The PORTAGE_DOCOMPRESS and
# PORTAGE_DOSTRIP arrays give docompress and dostrip something to append to;
# PORTAGE_DOSTRIP defaults to the image root unless RESTRICT bans stripping.
export MOPREFIX=${PN}
export PORTAGE_DOCOMPRESS_SIZE_LIMIT="128"
declare -a PORTAGE_DOCOMPRESS=( /usr/share/{doc,info,man} )
declare -a PORTAGE_DOCOMPRESS_SKIP=( "/usr/share/doc/${PF}/html" )
declare -a PORTAGE_DOSTRIP=()
declare -a PORTAGE_DOSTRIP_SKIP=()
if ! contains_word strip "${RESTRICT}"; then
    PORTAGE_DOSTRIP+=( / )
fi

# The image destination, prefix-aware. Falls back to D when ED is unset (for a
# non-prefix build or an EAPI without prefix variables).
__edest() {
    if ___eapi_has_prefix_variables && [[ -n ${ED} ]]; then
        printf '%s' "${ED%/}"
    else
        printf '%s' "${D%/}"
    fi
}

__strip_duplicate_slashes() {
    if [[ -n $1 ]]; then
        local removed=$1
        while [[ ${removed} == *//* ]]; do
            removed=${removed//\/\///}
        done
        printf '%s' "${removed}"
    fi
}

# --- assert / pipestatus ----------------------------------------------------

assert() {
    local x pipestatus=( "${PIPESTATUS[@]}" )
    ___eapi_has_assert || die "'${FUNCNAME}' banned in EAPI ${EAPI}"
    for x in "${pipestatus[@]}"; do
        [[ ${x} -eq 0 ]] || die "$@"
    done
}

if ___eapi_has_pipestatus; then
    pipestatus() {
        local status=( "${PIPESTATUS[@]}" )
        local s ret=0 verbose=""
        [[ ${1} == -v ]] && { verbose=1; shift; }
        [[ $# -ne 0 ]] && die "usage: pipestatus [-v]"
        for s in "${status[@]}"; do
            [[ ${s} -ne 0 ]] && ret=${s}
        done
        [[ ${verbose} && ${ret} -ne 0 ]] && echo "${status[@]}"
        return "${ret}"
    }
fi

# --- install-destination state ----------------------------------------------

into() {
    if [[ "$1" == "/" ]]; then
        export __E_DESTTREE=""
    else
        export __E_DESTTREE=$1
        local ED=$(__edest)
        if [[ ! -d "${ED}/${__E_DESTTREE#/}" ]]; then
            install -d "${ED}/${__E_DESTTREE#/}" || __helpers_die "${FUNCNAME[0]} failed"
        fi
    fi
    if ___eapi_has_DESTTREE_INSDESTTREE; then
        export DESTTREE=${__E_DESTTREE}
    fi
}

insinto() {
    if [[ "${1}" == "/" ]]; then
        export __E_INSDESTTREE=""
    else
        export __E_INSDESTTREE=${1}
        local ED=$(__edest)
        if [[ ! -d "${ED}/${__E_INSDESTTREE#/}" ]]; then
            install -d "${ED}/${__E_INSDESTTREE#/}" || __helpers_die "${FUNCNAME[0]} failed"
        fi
    fi
    if ___eapi_has_DESTTREE_INSDESTTREE; then
        export INSDESTTREE=${__E_INSDESTTREE}
    fi
}

exeinto() {
    if [[ "${1}" == "/" ]]; then
        export __E_EXEDESTTREE=""
    else
        export __E_EXEDESTTREE="${1}"
        local ED=$(__edest)
        if [[ ! -d "${ED}/${__E_EXEDESTTREE#/}" ]]; then
            install -d "${ED}/${__E_EXEDESTTREE#/}" || __helpers_die "${FUNCNAME[0]} failed"
        fi
    fi
}

docinto() {
    if [[ "${1}" == "/" ]]; then
        export __E_DOCDESTTREE=""
    else
        export __E_DOCDESTTREE="${1}"
    fi
}

insopts() {
    has -s "$@" && die "Never call insopts() with -s"
    export INSOPTIONS=$*
}
diropts() { export DIROPTIONS=$*; }
exeopts() {
    has -s "$@" && die "Never call exeopts() with -s"
    export EXEOPTIONS=$*
}
libopts() {
    ___eapi_has_dolib_libopts || die "'${FUNCNAME}' has been banned for EAPI '${EAPI}'"
    has -s "$@" && die "Never call libopts() with -s"
    export LIBOPTIONS=$*
}

# --- USE helpers ------------------------------------------------------------

useq() {
    ___eapi_has_useq || die "'${FUNCNAME}' banned in EAPI ${EAPI}"
    eqawarn "QA Notice: The 'useq' function is deprecated (replaced by 'use')"
    use ${1}
}

usev() {
    local nargs=1
    ___eapi_usev_has_second_arg && nargs=2
    [[ ${#} -gt ${nargs} ]] && die "usev takes at most ${nargs} arguments"
    if use ${1}; then
        echo "${2:-${1#!}}"
        return 0
    fi
    return 1
}

if ___eapi_has_usex; then
    usex() {
        if use "$1"; then echo "${2-yes}$4"; else echo "${3-no}$5"; fi
        return 0
    }
fi

use() {
    local invert u=$1
    if [[ ${u} == !* ]]; then
        u=${u:1}
        invert=1
    fi

    # Strict IUSE membership: from EAPI 5 a flag not in IUSE_EFFECTIVE is fatal;
    # earlier EAPIs only warn. Gated on PORTAGE_INTERNAL_CALLER like stock.
    if declare -F ___in_portage_iuse >/dev/null &&
        [[ -n ${EBUILD_PHASE} && -n ${PORTAGE_INTERNAL_CALLER} ]]; then
        if ! ___in_portage_iuse "${u}"; then
            if [[ ${EMERGE_FROM} != binary && ! ${EAPI} =~ ^(0|1|2|3|4)$ ]]; then
                die "USE Flag '${u}' not in IUSE for ${CATEGORY}/${PF}"
            fi
            eqawarn "QA Notice: USE Flag '${u}' not in IUSE for ${CATEGORY}/${PF}"
        fi
    fi

    contains_word "${u}" "${USE}"
    (( $? == invert ? 1 : 0 ))
}

use_with() {
    if [[ -z "${1}" ]]; then
        echo "!!! use_with() called without a parameter." >&2
        return 1
    fi
    local UW_SUFFIX
    if ___eapi_use_enable_and_use_with_support_empty_third_argument; then
        UW_SUFFIX=${3+=$3}
    else
        UW_SUFFIX=${3:+=$3}
    fi
    local UWORD=${2:-$1}
    if use ${1}; then echo "--with-${UWORD}${UW_SUFFIX}"; else echo "--without-${UWORD}"; fi
    return 0
}

use_enable() {
    if [[ -z "${1}" ]]; then
        echo "!!! use_enable() called without a parameter." >&2
        return 1
    fi
    local UE_SUFFIX
    if ___eapi_use_enable_and_use_with_support_empty_third_argument; then
        UE_SUFFIX=${3+=$3}
    else
        UE_SUFFIX=${3:+=$3}
    fi
    local UWORD=${2:-$1}
    if use ${1}; then echo "--enable-${UWORD}${UE_SUFFIX}"; else echo "--disable-${UWORD}"; fi
    return 0
}

if ___eapi_has_in_iuse; then
    in_iuse() {
        [[ $1 ]] || die "in_iuse() called without a parameter"
        contains_word "$1" "${IUSE_EFFECTIVE}"
    }
fi

if ___eapi_has_get_libdir; then
    get_libdir() {
        local libdir_var="LIBDIR_${ABI}"
        local libdir="lib"
        [[ -n ${ABI} && -n ${!libdir_var} ]] && libdir=${!libdir_var}
        echo "${libdir}"
    }
fi

# --- build helpers ----------------------------------------------------------

emake() {
    local emake_cmd=${MAKE:-make}
    ${emake_cmd} ${MAKEOPTS} ${EXTRA_EMAKE} "$@"
    local ret=$?
    if [[ ${ret} -ne 0 ]] && ___eapi_helpers_can_die; then
        __helpers_die "emake failed"
    fi
    return ${ret}
}

unpack() {
    local bzip2_cmd basename output srcdir suffix name f
    local -A suffix_by
    local -a suffixes

    (( $# == 0 )) && die "unpack: too few arguments (got 0; expected at least 1)"

    # The supported-suffix set, case-sensitively, per PMS 12.3.15. EAPI-gated
    # formats are appended only when the active EAPI accepts them.
    suffixes=(
        a
        bz
        bz2
        deb
        gz
        jar
        lzma
        tar
        tar.bz
        tar.bz2
        tar.gz
        tar.lzma
        tar.Z
        tbz
        tbz2
        tgz
        Z
        zip
        ZIP
    )
    ___eapi_unpack_supports_7z  && suffixes+=( 7z 7Z )
    ___eapi_unpack_supports_lha && suffixes+=( lha LHa LHA lzh )
    ___eapi_unpack_supports_rar && suffixes+=( rar RAR )
    ___eapi_unpack_supports_txz && suffixes+=( tar.xz txz )
    ___eapi_unpack_supports_xz  && suffixes+=( xz )

    # GNU tar auto-decompresses these compressed tarballs via
    # --warning=decompress-program (the tar|tar.*|tgz dispatch below). The zstd
    # and lz4 distfiles are now common in the Gentoo tree, so admit them to the
    # supported set rather than skipping them.
    suffixes+=( tar.lz tar.lzo tar.lz4 tar.zst )

    # Compose the finalised dictionary of supported suffixes. From EAPI 6 the
    # match is case-insensitive, induced by lowercasing every later assignment.
    if ! ___eapi_unpack_is_case_sensitive; then
        typeset -l suffix
    fi
    for suffix in "${suffixes[@]}"; do
        suffix_by[$suffix]=
    done

    # Honour the user's choice of bzip2 decompressor, if specified.
    for name in PORTAGE_BUNZIP2_CMD PORTAGE_BZIP2_CMD; do
        if [[ ${!name} == +([![:blank:]\"\']) ]]; then
            bzip2_cmd=${!name}
            break
        fi
    done

    for f; do
        # wrt PMS 12.3.15 Misc Commands
        if [[ ${f} != */* ]]; then
            srcdir=${DISTDIR}/
        elif [[ ${f} == ./* ]]; then
            srcdir=
        elif ___eapi_unpack_supports_absolute_paths; then
            srcdir=
            if [[ ${f} == "${DISTDIR%/}"/* ]]; then
                eqawarn "QA Notice: unpack called with redundant \${DISTDIR} in path"
            fi
        elif [[ ${f} == "${DISTDIR%/}"/* ]]; then
            die "Arguments to unpack() cannot begin with \${DISTDIR} in EAPI ${EAPI}"
        elif [[ ${f} == /* ]]; then
            die "Arguments to unpack() cannot be absolute in EAPI ${EAPI}"
        else
            die "Relative paths to unpack() must be prefixed with './' in EAPI ${EAPI}"
        fi

        if [[ ! -f ${srcdir}${f} ]]; then
            die "unpack: ${f@Q} either does not exist or is not a regular file"
        elif [[ ! -s ${srcdir}${f} ]]; then
            die "unpack: ${f@Q} cannot be unpacked because it is an empty file"
        fi

        # Extract the suffix, recognising multi-part tar.* suffixes.
        basename=${f##*/}
        suffix=
        if [[ ${basename} =~ \.([Tt][Aa][Rr]\.)?[^.]+$ ]]; then
            suffix=${BASH_REMATCH[0]#.}
        fi

        # Skip any file bearing an unsupported suffix instead of dying.
        if [[ ${suffix} && -v 'suffix_by[$suffix]' ]]; then
            __vecho ">>> Unpacking ${f@Q} to ${PWD}"
        else
            __vecho "=== Skipping unpack of ${f@Q}"
            continue
        fi

        case ${suffix,,} in
            7z)
                if ! output=$(7z x -y "${srcdir}${f}"); then
                    printf '%s\n' "${output}" >&2
                    false
                fi
                ;;
            a)
                ar x "${srcdir}${f}"
                ;;
            bz|bz2)
                "${bzip2_cmd-bzip2}" -dc -- "${srcdir}${f}" > "${basename%.*}"
                ;;
            deb)
                ar x "${srcdir}${f}"
                ;;
            gz|z)
                gzip -dc -- "${srcdir}${f}" > "${basename%.*}"
                ;;
            jar|zip)
                # unzip can prompt interactively on errors (bug #336285);
                # inducing EOF on stdin is an adequate countermeasure.
                unzip -qo "${srcdir}${f}" </dev/null
                ;;
            lha|lzh)
                lha xfq "${srcdir}${f}"
                ;;
            lzma)
                xz -F lzma -dc -- "${srcdir}${f}" > "${basename%.*}"
                ;;
            rar)
                unrar x -idq -o+ "${srcdir}${f}"
                ;;
            tar.bz|tar.bz2|tbz|tbz2)
                gtar -I "${bzip2_cmd-bzip2} -c" -xof "${srcdir}${f}"
                ;;
            tar|tar.*|tgz)
                # GNU tar recognises various compressors by suffix and runs the
                # appropriate decompressor; this handles tar.zst, tar.lz and more.
                gtar --warning=decompress-program -xof "${srcdir}${f}"
                ;;
            txz)
                gtar -xJof "${srcdir}${f}"
                ;;
            xz)
                xz -dc -- "${srcdir}${f}" > "${basename%.*}"
                ;;
        esac || die "unpack: failure unpacking ${f@Q}"
    done

    # PMS 12.3.15: make the freshly extracted top-level entries readable and
    # traversable so later phases can read the tree. '.' is left alone since it
    # is probably ${WORKDIR}. moraine has no chmod-lite on PATH, so the read-for-
    # all / traverse-for-directories mode is applied inline.
    find . -mindepth 1 -maxdepth 1 ! -type l -exec chmod -R a+rX {} +
}

econf() {
    local x
    local pid=${BASHPID}

    if ! ___eapi_has_prefix_variables; then
        local EPREFIX=
    fi

    __hasg() {
        local x s=$1; shift
        for x; do [[ ${x} == ${s} ]] && echo "${x}" && return 0; done
        return 1
    }
    __hasgq() { __hasg "$@" >/dev/null; }

    local phase_func=$(__ebuild_arg_to_phase "${EBUILD_PHASE}")
    if [[ -n ${phase_func} ]]; then
        if ! ___eapi_has_src_configure; then
            [[ ${phase_func} != src_compile ]] && \
                eqawarn "QA Notice: econf called in ${phase_func} instead of src_compile"
        else
            [[ ${phase_func} != src_configure ]] && \
                eqawarn "QA Notice: econf called in ${phase_func} instead of src_configure"
        fi
    fi

    : ${ECONF_SOURCE:=.}
    if [[ -x "${ECONF_SOURCE}/configure" ]]; then
        # Rewrite a #!/bin/sh shebang to CONFIG_SHELL, preserving the timestamp.
        if [[ -n ${CONFIG_SHELL} && \
            "$(head -n1 "${ECONF_SOURCE}/configure")" =~ ^'#!'[[:space:]]*/bin/sh([[:space:]]|$) ]]; then
            cp -p "${ECONF_SOURCE}/configure" "${ECONF_SOURCE}/configure._portage_tmp_.${pid}" || die
            sed -i \
                -e "1s:^#![[:space:]]*/bin/sh:#!${CONFIG_SHELL}:" \
                "${ECONF_SOURCE}/configure._portage_tmp_.${pid}" \
                || die "Substitution of shebang in '${ECONF_SOURCE}/configure' failed"
            touch -r "${ECONF_SOURCE}/configure" "${ECONF_SOURCE}/configure._portage_tmp_.${pid}" || die
            mv -f "${ECONF_SOURCE}/configure._portage_tmp_.${pid}" "${ECONF_SOURCE}/configure" || die
        fi

        # Refresh bundled config.guess/config.sub from the system gnuconfig copy,
        # replacing each atomically.
        if [[ -e "${EPREFIX}"/usr/share/gnuconfig/ ]]; then
            find "${WORKDIR}" -type f '(' \
                -name config.guess -o -name config.sub ')' -print0 | \
            while read -r -d $'\0' x; do
                __vecho " * econf: updating ${x/${WORKDIR}\/} with ${EPREFIX}/usr/share/gnuconfig/${x##*/}"
                cp -f "${EPREFIX}"/usr/share/gnuconfig/"${x##*/}" "${x}.${pid}"
                mv -f "${x}.${pid}" "${x}"
            done
        fi

        local conf_args=()
        if ___eapi_econf_passes_--disable-dependency-tracking || ___eapi_econf_passes_--disable-silent-rules || ___eapi_econf_passes_--docdir_and_--htmldir || ___eapi_econf_passes_--with-sysroot; then
            local conf_help=$("${ECONF_SOURCE}/configure" --help 2>/dev/null)

            if ___eapi_econf_passes_--datarootdir; then
                [[ ${conf_help} == *--datarootdir* ]] && \
                    conf_args+=( --datarootdir="${EPREFIX}"/usr/share )
            fi
            if ___eapi_econf_passes_--disable-dependency-tracking; then
                [[ ${conf_help} == *--disable-dependency-tracking[^A-Za-z0-9+_.-]* ]] && \
                    conf_args+=( --disable-dependency-tracking )
            fi
            if ___eapi_econf_passes_--disable-silent-rules; then
                [[ ${conf_help} == *--disable-silent-rules[^A-Za-z0-9+_.-]* ]] && \
                    conf_args+=( --disable-silent-rules )
            fi
            if ___eapi_econf_passes_--disable-static; then
                [[ ${conf_help} == *--enable-shared[^A-Za-z0-9+_.-]* && \
                    ${conf_help} == *--enable-static[^A-Za-z0-9+_.-]* ]] && \
                    conf_args+=( --disable-static )
            fi
            if ___eapi_econf_passes_--docdir_and_--htmldir; then
                [[ ${conf_help} == *--docdir* ]] && \
                    conf_args+=( --docdir="${EPREFIX}/usr/share/doc/${PF}" )
                [[ ${conf_help} == *--htmldir* ]] && \
                    conf_args+=( --htmldir="${EPREFIX}/usr/share/doc/${PF}/html" )
            fi
            if ___eapi_econf_passes_--with-sysroot; then
                [[ ${conf_help} == *--with-sysroot[^A-Za-z0-9+_.-]* ]] && \
                    conf_args+=( --with-sysroot="${ESYSROOT:-/}" )
            fi
        fi

        local libdir libdir_var="LIBDIR_${ABI}"
        [[ -n ${ABI} && -n ${!libdir_var} ]] && libdir=${!libdir_var}
        if [[ -n ${libdir} ]] && ! __hasgq --libdir=\* "$@"; then
            local CONF_PREFIX=$(__hasg --exec-prefix=\* "$@")
            [[ -z ${CONF_PREFIX} ]] && CONF_PREFIX=$(__hasg --prefix=\* "$@")
            : ${CONF_PREFIX:=${EPREFIX}/usr}
            CONF_PREFIX=${CONF_PREFIX#*=}
            [[ ${CONF_PREFIX} != /* ]] && CONF_PREFIX="/${CONF_PREFIX}"
            [[ ${libdir} != /* ]] && libdir="/${libdir}"
            conf_args+=( --libdir="$(__strip_duplicate_slashes "${CONF_PREFIX}${libdir}")" )
        fi

        eval "local -a EXTRA_ECONF=(${EXTRA_ECONF})"

        set -- \
            --prefix="${EPREFIX}"/usr \
            ${CBUILD:+--build=${CBUILD}} \
            --host=${CHOST} \
            ${CTARGET:+--target=${CTARGET}} \
            --mandir="${EPREFIX}"/usr/share/man \
            --infodir="${EPREFIX}"/usr/share/info \
            --datadir="${EPREFIX}"/usr/share \
            --sysconfdir="${EPREFIX}"/etc \
            --localstatedir="${EPREFIX}"/var/lib \
            "${conf_args[@]}" \
            "$@" \
            "${EXTRA_ECONF[@]}"
        __vecho "${ECONF_SOURCE}/configure" "$@"

        if ! "${ECONF_SOURCE}/configure" "$@"; then
            ___eapi_helpers_can_die || die "econf failed"
            __helpers_die "econf failed"
            return 1
        fi
    elif [[ -f "${ECONF_SOURCE}/configure" ]]; then
        die "configure is not executable"
    else
        die "no configure script found"
    fi
}

# --- patch helpers ----------------------------------------------------------

if ___eapi_has_eapply; then
    __eapply_patch() {
        local prefix=$1 patch=$2 output IFS
        shift 2
        ebegin "${prefix:-Applying }${patch##*/}"
        set -- -p1 -f -g0 --no-backup-if-mismatch "$@"
        if output=$(LC_ALL= LC_MESSAGES=C patch "$@" < "${patch}" 2>&1); then
            if [[ ${output} == *[0-9]' with fuzz '[0-9]* ]]; then
                printf '%s\n' "${output}"
            fi
            eend 0
        else
            printf '%s\n' "${output}" >&2
            eend 1
            __helpers_die "patch ${*@Q} failed with ${patch@Q}"
        fi
    }

    eapply() {
        local LC_ALL LC_COLLATE=C f i path
        local -a operands options

        while (( $# )); do
            case $1 in
                --) break ;;
                *)  options+=("$1") ;;
            esac
            shift
        done

        if (( $# )); then
            shift
            operands=("$@")
        else
            set -- "${options[@]}"
            options=()
            while (( $# )); do
                case $1 in
                    -*)
                        if (( ! ${#operands[@]} )); then
                            options+=("$1")
                        else
                            die "eapply: options must precede non-option arguments"
                        fi
                        ;;
                    *) operands+=("$1") ;;
                esac
                shift
            done
        fi

        (( ! ${#operands[@]} )) && die "eapply: no operands were specified"

        for path in "${operands[@]}"; do
            if [[ -d ${path} ]]; then
                i=0
                for f in "${path}"/*; do
                    if [[ ${f} == *.@(diff|patch) ]]; then
                        (( i++ == 0 )) && einfo "Applying patches from ${path} ..."
                        __eapply_patch '  ' "${f}" "${options[@]}" || return
                    fi
                done
                (( i == 0 )) && die "No *.{patch,diff} files in directory ${path}"
            else
                __eapply_patch '' "${path}" "${options[@]}" || return
            fi
        done
    }
fi

if ___eapi_has_eapply_user; then
    eapply_user() {
        local basename basedir tagfile d f
        local -A patch_by

        [[ ${EBUILD_PHASE} == prepare ]] || \
            die "eapply_user() called during invalid phase: ${EBUILD_PHASE}"

        tagfile=${T}/.portage_user_patches_applied
        [[ -f ${tagfile} ]] && return
        >> "${tagfile}"

        basedir=${PORTAGE_CONFIGROOT%/}/etc/portage/patches

        for d in "${basedir}"/"${CATEGORY}"/{"${PN}","${P}","${P}-${PR}"}{,":${SLOT%/*}"}; do
            [[ -d ${d} ]] || continue
            for f in "${d}"/*; do
                if [[ ${f} == *.@(diff|patch) ]]; then
                    basename=${f##*/}
                    if [[ -s ${f} ]]; then
                        patch_by[$basename]=${f}
                    else
                        unset -v 'patch_by[$basename]'
                    fi
                fi
            done
        done

        if (( ${#patch_by[@]} > 0 )); then
            einfo "Applying user patches from ${basedir} ..."
            while IFS= read -rd '' basename; do
                eapply -- "${patch_by[$basename]}"
            done < <(printf '%s\0' "${!patch_by[@]}" | LC_ALL=C sort -z)
            einfo "User patches applied."
        fi
    }
fi

if ___eapi_has_einstalldocs; then
    einstalldocs() (
        local f
        if [[ ! ${DOCS@A} ]]; then
            for f in README* ChangeLog AUTHORS NEWS TODO CHANGES \
                THANKS BUGS FAQ CREDITS CHANGELOG; do
                [[ -f ${f} && -s ${f} ]] && { docinto / && dodoc "${f}"; }
            done
        elif [[ ${DOCS@a} == *a* ]] && (( ${#DOCS[@]} )); then
            docinto / && dodoc -r "${DOCS[@]}"
        elif [[ ${DOCS@a} != *[aA]* && ${DOCS} ]]; then
            docinto / && dodoc -r ${DOCS}
        fi

        if [[ ${HTML_DOCS@a} == *a* ]] && (( ${#HTML_DOCS[@]} )); then
            docinto html && dodoc -r "${HTML_DOCS[@]}"
        elif [[ ${HTML_DOCS@a} != *[aA]* && ${HTML_DOCS} ]]; then
            docinto html && dodoc -r ${HTML_DOCS}
        fi
    )
fi

# --- do*/new* install family ------------------------------------------------

dodir() {
    local ED=$(__edest)
    install -d ${DIROPTIONS} "${@/#/${ED}/}" || __helpers_die "${FUNCNAME} failed"
}

dobin() {
    local ED=$(__edest) desttree=${__E_DESTTREE-/usr}
    install -d "${ED}/${desttree#/}/bin"
    install -m0755 "$@" "${ED}/${desttree#/}/bin/" || __helpers_die "${FUNCNAME} failed"
}

dosbin() {
    local ED=$(__edest) desttree=${__E_DESTTREE-/usr}
    install -d "${ED}/${desttree#/}/sbin"
    install -m0755 "$@" "${ED}/${desttree#/}/sbin/" || __helpers_die "${FUNCNAME} failed"
}

doexe() {
    local ED=$(__edest)
    local dir="${ED}/${__E_EXEDESTTREE#/}"
    install -d "${dir}"
    install ${EXEOPTIONS:--m0755} "$@" "${dir}/" || __helpers_die "${FUNCNAME} failed"
}

# Install one file into the destination directory, normalizing its mode to
# INSOPTIONS. A symlink source is recreated as a symlink for EAPI 4 and later
# and otherwise dereferenced, matching bin/doins.py _doins.
__doins_file() {
    local dir=$1 src=$2
    if [[ -L ${src} ]] && ___eapi_doins_and_newins_preserve_symlinks; then
        cp -P "${src}" "${dir}/" || __helpers_die "doins failed"
    else
        install ${INSOPTIONS:--m0644} "${src}" "${dir}/" || __helpers_die "doins failed"
    fi
}

# Recreate a source directory under the destination, descending into it and
# normalizing every installed file's mode, matching doins.py recursion.
__doins_dir() {
    local dir=$1 src=${2%/}
    local dest="${dir}/${src##*/}"
    install -d "${dest}" || __helpers_die "doins failed"
    local entry restore_nullglob restore_dotglob
    restore_nullglob=$(shopt -p nullglob)
    restore_dotglob=$(shopt -p dotglob)
    shopt -s nullglob dotglob
    for entry in "${src}"/*; do
        if [[ -d ${entry} && ! -L ${entry} ]]; then
            __doins_dir "${dest}" "${entry}"
        else
            __doins_file "${dest}" "${entry}"
        fi
    done
    ${restore_nullglob}
    ${restore_dotglob}
}

doins() {
    local ED=$(__edest) recursive=
    [[ $1 == -r ]] && { recursive=1; shift; }
    local dir="${ED}/${__E_INSDESTTREE#/}"
    install -d "${dir}" || __helpers_die "${FUNCNAME} failed"
    local f
    for f in "$@"; do
        if [[ -d ${f} && ! -L ${f} ]]; then
            [[ -n ${recursive} ]] || { __helpers_die "doins: ${f} is a directory; use -r"; continue; }
            __doins_dir "${dir}" "${f}"
        else
            __doins_file "${dir}" "${f}"
        fi
    done
}

dolib() {
    local ED=$(__edest) libdir desttree=${__E_DESTTREE-/usr} x ret=0
    libdir=$(get_libdir 2>/dev/null) || libdir=lib
    local dir="${ED}/${desttree#/}/${libdir}"
    install -d "${dir}"
    for x in "$@"; do
        if [[ -e ${x} ]]; then
            # Recreate a symlink source as a symlink so VDB CONTENTS keeps a sym
            # entry; install only regular files. Mirrors bin/ebuild-helpers/dolib.
            if [[ ! -L ${x} ]]; then
                install ${LIBOPTIONS:--m0644} "${x}" "${dir}/"
            else
                ln -s "$(readlink "${x}")" "${dir}/${x##*/}"
            fi
        else
            echo "!!! ${FUNCNAME}: ${x} does not exist" >&2
            false
        fi
        (( ret |= $? ))
    done
    (( ret != 0 )) && __helpers_die "${FUNCNAME} failed"
    return ${ret}
}
dolib.a() { LIBOPTIONS="-m0644" dolib "$@"; }
dolib.so() { LIBOPTIONS="-m0755" dolib "$@"; }

doheader() {
    ___eapi_has_doheader || die "'${FUNCNAME}' not supported in this EAPI"
    local ED=$(__edest) recursive=
    [[ $1 == -r ]] && { recursive=1; shift; }
    insinto /usr/include
    if [[ -n ${recursive} ]]; then
        doins -r "$@"
    else
        doins "$@"
    fi
}

doinfo() {
    [[ -n $1 ]] || { __helpers_die "doinfo: at least one argument needed"; return 1; }
    local ED=$(__edest)
    if [[ ! -d ${ED}/usr/share/info ]]; then
        install -d "${ED}/usr/share/info" || \
            { __helpers_die "doinfo: failed to install ${ED}/usr/share/info"; return 1; }
    fi
    install -m0644 "$@" "${ED}/usr/share/info"
    local rval=$?
    if (( rval != 0 )); then
        local x
        for x in "$@"; do
            [[ -e ${x} ]] || echo "!!! doinfo: ${x} does not exist" >&2
        done
        __helpers_die "doinfo failed"
    fi
    return ${rval}
}

doman() {
    (( $# >= 1 )) || { __helpers_die "doman: at least one argument needed"; return 1; }
    local ED=$(__edest) i18n="" ret=0
    local x suffix realname name mandir
    for x in "$@"; do
        if [[ ${x:0:6} == "-i18n=" ]]; then
            i18n=${x:6}/
            continue
        fi
        if [[ ${x:0:6} == ".keep_" ]]; then
            continue
        fi

        suffix=${x##*.}

        # A compressed man page is not portable; warn and re-derive the suffix.
        if [[ ${suffix} == @(Z|gz|bz2) ]]; then
            eqawarn "QA Notice: doman argument '${x}' is compressed, this is not portable"
            realname=${x%.*}
            suffix=${realname##*.}
        fi

        if [[ ${EAPI} == [23] ]] || [[ -z ${i18n} ]] && [[ ${EAPI:-0} != [01] ]] && [[ ${x} =~ (.*)\.([a-z][a-z](_[A-Z][A-Z])?)\.(.*) ]]; then
            name=${BASH_REMATCH[1]##*/}.${BASH_REMATCH[4]}
            mandir=${BASH_REMATCH[2]}/man${suffix:0:1}
        else
            name=${x##*/}
            mandir=${i18n#/}man${suffix:0:1}
        fi

        if [[ ${mandir} == *man[0-9n] ]]; then
            if [[ -s ${x} ]]; then
                if [[ ! -d ${ED}/usr/share/man/${mandir} ]]; then
                    install -d "${ED}/usr/share/man/${mandir}"
                fi
                install -m0644 "${x}" "${ED}/usr/share/man/${mandir}/${name}"
                (( ret |= $? ))
            elif [[ ! -e ${x} ]]; then
                echo "!!! doman: ${x} does not exist" >&2
                (( ret |= 1 ))
            fi
        else
            __vecho "doman: '${x}' is probably not a man page; skipping" >&2
            (( ret |= 1 ))
        fi
    done

    (( ret != 0 )) && __helpers_die "doman failed"
    return ${ret}
}

dodoc() {
    local ED=$(__edest) recursive=
    [[ $1 == -r ]] && { recursive=1; shift; }
    local dir="${ED}/usr/share/doc/${PF}/${__E_DOCDESTTREE#/}"
    install -d "${dir}"
    local f
    for f in "$@"; do
        if [[ -d ${f} ]]; then
            [[ -n ${recursive} ]] || { __helpers_die "dodoc: ${f} is a directory; use -r"; continue; }
            cp -R "${f}" "${dir}/" || __helpers_die "${FUNCNAME} failed"
        else
            install -m0644 "${f}" "${dir}/" || __helpers_die "${FUNCNAME} failed"
        fi
    done
}

dosym() {
    local option_r=
    if ___eapi_has_dosym_r && [[ $1 == -r ]]; then
        option_r=t
        shift
    fi

    [[ $# -eq 2 ]] || { __helpers_die "dosym: two arguments needed"; return 1; }

    local ED=$(__edest)

    if [[ ${2} == */ ]] || [[ -d ${ED}/${2#/} && ! -L ${ED}/${2#/} ]]; then
        # implicit basename not allowed by PMS (bug #379899)
        __helpers_die "dosym: dosym target omits basename: '${2}'"
        return 1
    fi

    local target=${1}

    if [[ ${option_r} ]]; then
        # Transparent bash-only replacement for GNU "realpath -m -s". Resolves
        # "/./", "/../" and extra "/" characters without touching any file.
        dosym_canonicalize() {
            local path slash i prev out IFS=/

            read -r -d '' -a path < <(printf '%s\0' "${1}")
            [[ ${1} == /* ]] && slash=/

            while true; do
                prev=
                for i in "${!path[@]}"; do
                    if [[ -z ${path[i]} || ${path[i]} == . ]]; then
                        unset "path[i]"
                    elif [[ ${path[i]} != .. ]]; then
                        prev=${i}
                    elif [[ ${prev} || ${slash} ]]; then
                        [[ ${prev} ]] && unset "path[prev]"
                        unset "path[i]"
                        continue 2
                    fi
                done
                break
            done

            out="${slash}${path[*]}"
            printf "%s\n" "${out:-.}"
        }

        # Expansion makes sense only for an absolute target path.
        [[ ${target} == /* ]] || { __helpers_die \
            "dosym: -r specified but no absolute target path: '${target}'"; return 1; }

        target=$(dosym_canonicalize "${target}")
        local linkdir comp
        linkdir=$(dosym_canonicalize "/${2#/}")
        linkdir=${linkdir%/*}
        linkdir=${linkdir:-/}

        local IFS=/
        for comp in ${linkdir}; do
            if [[ ${target%%/*} == "${comp}" ]]; then
                target=${target#"${comp}"}
                target=${target#/}
            else
                target=..${target:+/}${target}
            fi
        done
        unset IFS
        target=${target:-.}
    fi

    local destdir=${2%/*}
    [[ ! -d ${ED}/${destdir#/} ]] && dodir "${destdir}"
    ln -snf "${target}" "${ED}/${2#/}" || __helpers_die "${FUNCNAME} failed"
}

doinitd() {
    local ED=$(__edest)
    install -d "${ED}/etc/init.d"
    install ${EXEOPTIONS:--m0755} "$@" "${ED}/etc/init.d/" || __helpers_die "${FUNCNAME} failed"
}
doconfd() {
    local ED=$(__edest)
    install -d "${ED}/etc/conf.d"
    install ${INSOPTIONS:--m0644} "$@" "${ED}/etc/conf.d/" || __helpers_die "${FUNCNAME} failed"
}
doenvd() {
    local ED=$(__edest)
    install -d "${ED}/etc/env.d"
    install ${INSOPTIONS:--m0644} "$@" "${ED}/etc/env.d/" || __helpers_die "${FUNCNAME} failed"
}

domo() {
    ___eapi_has_domo || die "'${FUNCNAME}' not supported in this EAPI"
    (( $# >= 1 )) || { __helpers_die "domo: at least one argument needed"; return 1; }
    local ED=$(__edest) x mytiny mydir ret=0

    # EAPI 0 through 6 honor into; later EAPIs force /usr like the other
    # /usr/share helpers. An unset __E_DESTTREE defaults to /usr; an empty value
    # (set by `into /`) is kept so it routes to the image root.
    local desttree
    if ___eapi_domo_respects_into; then
        desttree=${__E_DESTTREE-/usr}
    else
        desttree=/usr
    fi

    install -d "${ED}/${desttree#/}/share/locale"
    for x in "$@"; do
        if [[ -e ${x} ]]; then
            mytiny=${x##*/}
            mydir="${ED}/${desttree#/}/share/locale/${mytiny%.*}/LC_MESSAGES"
            install -d "${mydir}"
            install -m0644 "${x}" "${mydir}/${MOPREFIX}.mo"
        else
            echo "!!! domo: ${x} does not exist" >&2
            false
        fi
        (( ret |= $? ))
    done

    (( ret != 0 )) && __helpers_die "domo failed"
    return ${ret}
}

keepdir() {
    ___eapi_has_strict_keepdir && [[ $# -eq 0 ]] && die "keepdir: at least one argument needed"
    local ED=$(__edest) d
    dodir "$@"
    for d in "$@"; do
        >> "${ED}/${d#/}/.keep_${CATEGORY}_${PN}-${SLOT%/*}" || \
            __helpers_die "${FUNCNAME} failed"
    done
}

fperms() {
    local ED=$(__edest) arg got_mode=
    local -a args=()
    for arg in "$@"; do
        # A leading dash is an option unless it is a single symbolic mode
        # character, in which case it is the mode. Mirrors bin/ebuild-helpers/fperms.
        if [[ ${arg} == -* && ${arg} != -[ugorwxXst] ]]; then
            args+=( "${arg}" )
        elif [[ ! ${got_mode} ]]; then
            got_mode=1
            args+=( "${arg}" )
        else
            args+=( "${ED}/${arg#/}" )
        fi
    done
    chmod "${args[@]}" || __helpers_die "${FUNCNAME} failed"
}

# Resolve a symbolic fowners owner spec to a numeric uid:gid against the target
# root account database, mirroring bin/ebuild-helpers/fowners __resolve_owner. A
# numeric user or group passes through unchanged. The colon-separated passwd and
# group fields are parsed with a pure-bash read loop rather than awk.
__resolve_owner() {
    local owner=$1 pwdb_path=${2}/etc
    local user group uid gid pgid name pw num grp

    IFS=':' read -r user group <<< "${owner}"

    if [[ -n ${user} ]]; then
        if [[ ${user} =~ ^[0-9]+$ ]]; then
            uid=${user}
        else
            while IFS=: read -r name pw num grp _; do
                if [[ ${name} == "${user}" ]]; then
                    uid=${num}
                    pgid=${grp}
                    break
                fi
            done < "${pwdb_path}/passwd"
            [[ ${uid} =~ ^[0-9]+$ ]] || \
                __helpers_die "fowners: invalid user in ${pwdb_path}/passwd: ${owner}"
        fi
    fi

    if [[ -n ${group} ]]; then
        if [[ ${group} =~ ^[0-9]+$ ]]; then
            gid=${group}
        else
            while IFS=: read -r name pw num _; do
                if [[ ${name} == "${group}" ]]; then
                    gid=${num}
                    break
                fi
            done < "${pwdb_path}/group"
            [[ ${gid} =~ ^[0-9]+$ ]] || \
                __helpers_die "fowners: invalid group in ${pwdb_path}/group: ${owner}"
        fi
        gid=":${gid}"
    elif [[ ${owner} == *: ]]; then
        # `chown uid:` resolves the group to the user's primary group.
        if [[ ${pgid} =~ ^[0-9]+$ ]]; then
            gid=":${pgid}"
        else
            __helpers_die "fowners: invalid primary group for ${user} in ${pwdb_path}/passwd: ${owner}"
        fi
    fi

    printf '%s%s' "${uid}" "${gid}"
}

fowners() {
    local ED=$(__edest) arg got_owner= eroot pwdb_root pwdb_eroot
    local -a args=()

    if ___eapi_has_prefix_variables; then
        eroot=${EROOT}
    else
        eroot=${ROOT}
    fi

    if ___eapi_has_SYSROOT && [[ ${EBUILD_PHASE} == install ]]; then
        pwdb_root=${SYSROOT}
        pwdb_eroot=${ESYSROOT}
    else
        pwdb_root=${ROOT%/}
        pwdb_eroot=${eroot%/}
    fi

    for arg in "$@"; do
        if [[ ${arg} == -* ]]; then
            args+=( "${arg}" )
        elif [[ ! ${got_owner} ]]; then
            got_owner=1
            # For a cross-root install resolve the owner against the target
            # account database; ROOT=/ passes the owner straight through.
            if [[ -n ${pwdb_root} ]]; then
                args+=( "$(__resolve_owner "${arg}" "${pwdb_eroot}")" )
            else
                args+=( "${arg}" )
            fi
        else
            args+=( "${ED}/${arg#/}" )
        fi
    done

    chown "${args[@]}" || __helpers_die "${FUNCNAME} failed"
}

# Record paths whose docs the (future) compression stage should compress, or
# with -x exclude from compression. Mirrors bin/phase-helpers.sh docompress.
docompress() {
    ___eapi_has_docompress || die "'docompress' not supported in this EAPI"

    local f g
    if [[ ${1} == "-x" ]]; then
        shift
        for f; do
            f=$(__strip_duplicate_slashes "${f}"); f=${f%/}
            [[ ${f:0:1} == / ]] || f="/${f}"
            for g in "${PORTAGE_DOCOMPRESS_SKIP[@]}"; do
                [[ ${f} == "${g}" ]] && continue 2
            done
            PORTAGE_DOCOMPRESS_SKIP+=( "${f}" )
        done
    else
        for f; do
            f=$(__strip_duplicate_slashes "${f}"); f=${f%/}
            [[ ${f:0:1} == / ]] || f="/${f}"
            for g in "${PORTAGE_DOCOMPRESS[@]}"; do
                [[ ${f} == "${g}" ]] && continue 2
            done
            PORTAGE_DOCOMPRESS+=( "${f}" )
        done
    fi
}

# Record paths eligible for stripping, or with -x exclude them. The strip stage
# consumes PORTAGE_DOSTRIP/PORTAGE_DOSTRIP_SKIP. Mirrors phase-helpers.sh dostrip.
dostrip() {
    ___eapi_has_dostrip || die "'${FUNCNAME}' not supported in this EAPI"

    local f g
    if [[ $1 == "-x" ]]; then
        shift
        for f; do
            f=$(__strip_duplicate_slashes "${f}"); f=${f%/}
            [[ ${f:0:1} == / ]] || f="/${f}"
            for g in "${PORTAGE_DOSTRIP_SKIP[@]}"; do
                [[ ${f} == "${g}" ]] && continue 2
            done
            PORTAGE_DOSTRIP_SKIP+=( "${f}" )
        done
    else
        for f; do
            f=$(__strip_duplicate_slashes "${f}"); f=${f%/}
            [[ ${f:0:1} == / ]] || f="/${f}"
            for g in "${PORTAGE_DOSTRIP[@]}"; do
                [[ ${f} == "${g}" ]] && continue 2
            done
            PORTAGE_DOSTRIP+=( "${f}" )
        done
    fi
}

if ___eapi_has_dosed; then
    dosed() {
        local ED=$(__edest) expr="s:${ED}::g" f
        for f in "$@"; do
            if [[ ${f} == -* ]] || [[ ${f} != /* && ${f} == *:* ]]; then
                expr=${f}
            else
                sed -i -e "${expr}" "${ED}/${f#/}" || __helpers_die "${FUNCNAME} failed"
                expr="s:${ED}::g"
            fi
        done
    }
fi

if ___eapi_has_dohard; then
    dohard() {
        [[ $# -eq 2 ]] || { __helpers_die "dohard: two arguments needed"; return 1; }
        local ED=$(__edest) destdir=${2%/*}
        [[ ! -d ${ED}/${destdir#/} ]] && dodir "${destdir}"
        ln -f "${ED}/${1#/}" "${ED}/${2#/}" || __helpers_die "${FUNCNAME} failed"
    }
fi

# new* copy a single source to a temporary file under the new name, then defer
# to the matching do* helper.
__new_helper() {
    local helper=$1 newname=$2 src=$3
    [[ -e ${src} || ${src} == - ]] || __helpers_die "${helper%% *}: ${src} does not exist"
    local tmp="${T}/${newname}"
    if [[ ${src} == - ]]; then
        ___eapi_newins_supports_reading_from_standard_input || \
            die "${helper%% *}: reading from stdin not supported in this EAPI"
        cat > "${tmp}"
    else
        # -P preserves a symlink source as a symlink so newins/newexe honor the
        # same symlink-preservation predicate as doins for EAPI 4 and later.
        cp -Pf "${src}" "${tmp}" || __helpers_die "${helper%% *} failed"
    fi
    ${helper} "${tmp}"
}
newbin()    { __new_helper dobin    "$2" "$1"; }
newsbin()   { __new_helper dosbin   "$2" "$1"; }
newexe()    { __new_helper doexe    "$2" "$1"; }
newins()    { __new_helper doins    "$2" "$1"; }
newlib.a()  { __new_helper dolib.a  "$2" "$1"; }
newlib.so() { __new_helper dolib.so "$2" "$1"; }
newman()    { __new_helper doman    "$2" "$1"; }
newheader() { __new_helper doheader "$2" "$1"; }
newdoc()    { __new_helper dodoc    "$2" "$1"; }
newinitd()  { __new_helper doinitd  "$2" "$1"; }
newconfd()  { __new_helper doconfd  "$2" "$1"; }
newenvd()   { __new_helper doenvd   "$2" "$1"; }

# --- EAPI-versioned default phase implementations ---------------------------

__eapi0_pkg_nofetch() {
    [[ -z ${A} ]] && return
    elog "The following files cannot be fetched for ${PN}:"
    local x
    for x in ${A}; do elog "   ${x}"; done
}

__eapi0_src_unpack() {
    [[ -n ${A} ]] && unpack ${A}
}

__eapi0_src_compile() {
    [[ -x ./configure ]] && econf
    __eapi2_src_compile
}

__eapi0_src_test() {
    [[ -n ${MAKEFLAGS} ]] && local MAKEOPTS=""
    local emake_cmd="${MAKE:-make} ${MAKEOPTS} ${EXTRA_EMAKE}"
    local internal_opts=
    ___eapi_default_src_test_disables_parallel_jobs && internal_opts+=" -j1"
    if ${emake_cmd} ${internal_opts} check -n &> /dev/null; then
        ${emake_cmd} ${internal_opts} check || die "Make check failed. See above for details."
    elif ${emake_cmd} ${internal_opts} test -n &> /dev/null; then
        ${emake_cmd} ${internal_opts} test || die "Make test failed. See above for details."
    fi
}

__eapi1_src_compile() {
    __eapi2_src_configure
    __eapi2_src_compile
}

__eapi2_src_prepare() { :; }

__eapi2_src_configure() {
    [[ -x ${ECONF_SOURCE:-.}/configure ]] && econf
}

__eapi2_src_compile() {
    if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
        emake || die "emake failed"
    fi
}

__eapi4_src_install() {
    if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
        emake DESTDIR="${D}" install
    fi
    if ! declare -p DOCS &>/dev/null; then
        local d
        for d in README* ChangeLog AUTHORS NEWS TODO CHANGES \
                THANKS BUGS FAQ CREDITS CHANGELOG; do
            [[ -s "${d}" ]] && dodoc "${d}"
        done
    elif [[ ${DOCS@a} == *a* ]]; then
        dodoc "${DOCS[@]}"
    else
        dodoc ${DOCS}
    fi
}

__eapi6_src_prepare() {
    if [[ ${PATCHES@a} == *a* ]]; then
        [[ ${#PATCHES[@]} -gt 0 ]] && eapply "${PATCHES[@]}"
    elif [[ -n ${PATCHES} ]]; then
        eapply ${PATCHES}
    fi
    eapply_user
}

__eapi6_src_install() {
    if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
        emake DESTDIR="${D}" install
    fi
    einstalldocs
}

__eapi8_src_prepare() {
    local f
    if [[ ${PATCHES@a} == *a* ]]; then
        [[ ${#PATCHES[@]} -gt 0 ]] && eapply -- "${PATCHES[@]}"
    elif [[ -n ${PATCHES} ]]; then
        eapply -- ${PATCHES}
    fi
    eapply_user
}

# --- IPC-backed version queries (moraine MORAINE_IPC_HELPER patch) ----------
# These call the moraine IPC helper, which writes a request to the FIFO under
# .ipc and reads the response. The Rust manager answers from the installed store
# and repository. The helper path is exported by the driver as MORAINE_IPC_HELPER.
# The request is "<op> <root> <atom> <USE...>"; the caller USE lets the manager
# evaluate USE-conditional dependencies in the atom. This mirrors
# ___best_version_and_has_version_common in stock Portage.

___moraine_best_version_and_has_version_common() {
    local atom root root_arg

    case $1 in
        --host-root|-r|-d|-b)
            root_arg=$1
            shift ;;
    esac
    atom=$1
    shift
    [[ $# -gt 0 ]] && die "${FUNCNAME[1]}: unused argument(s): $*"

    case ${root_arg} in
        "")
            if ___eapi_has_prefix_variables; then
                root=${ROOT%/}/${EPREFIX#/}
            else
                root=${ROOT}
            fi ;;
        --host-root)
            if ! ___eapi_best_version_and_has_version_support_--host-root; then
                die "${FUNCNAME[1]}: option ${root_arg} is not supported with EAPI ${EAPI}"
            fi
            if ___eapi_has_prefix_variables; then
                root=/${BROOT#/}
            else
                root=/
            fi ;;
        -r|-d|-b)
            if ! ___eapi_best_version_and_has_version_support_-b_-d_-r; then
                die "${FUNCNAME[1]}: option ${root_arg} is not supported with EAPI ${EAPI}"
            fi
            if ___eapi_has_prefix_variables; then
                case ${root_arg} in
                    -r) root=${ROOT%/}/${EPREFIX#/} ;;
                    -d) root=${ESYSROOT:-/} ;;
                    -b) root=/${BROOT#/} ;;
                esac
            else
                case ${root_arg} in
                    -r) root=${ROOT:-/} ;;
                    -d) root=${SYSROOT:-/} ;;
                    -b) root=/ ;;
                esac
            fi ;;
    esac

    "${MORAINE_IPC_HELPER}" "${FUNCNAME[1]}" "${root}" "${atom}" ${USE}
    local retval=$?

    case "${retval}" in
        0|1)
            return ${retval} ;;
        2)
            die "${FUNCNAME[1]}: invalid atom: ${atom}" ;;
        *)
            die "${FUNCNAME[1]}: unexpected helper exit code: ${retval}" ;;
    esac
}

has_version() {
    ___moraine_best_version_and_has_version_common "$@"
}

best_version() {
    ___moraine_best_version_and_has_version_common "$@"
}
