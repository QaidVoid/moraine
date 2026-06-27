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
    local file src
    (( $# == 0 )) && die "unpack: too few arguments"
    for file in "$@"; do
        if [[ ${file} == ./* ]]; then
            src=${file}
        elif [[ ${file} == /* ]]; then
            ___eapi_unpack_supports_absolute_paths || \
                die "unpack: absolute paths not supported in EAPI ${EAPI}: ${file}"
            src=${file}
        else
            src="${DISTDIR}/${file}"
        fi
        [[ -e ${src} ]] || die "unpack: file does not exist: ${src}"
        case ${file} in
            *.tar)            tar xf "${src}" || die "unpack failed: ${file}" ;;
            *.tar.gz|*.tgz)   tar xzf "${src}" || die "unpack failed: ${file}" ;;
            *.tar.bz2|*.tbz2|*.tbz) tar xjf "${src}" || die "unpack failed: ${file}" ;;
            *.tar.xz|*.txz)   tar xJf "${src}" || die "unpack failed: ${file}" ;;
            *.tar.lz)         tar --lzip -xf "${src}" || die "unpack failed: ${file}" ;;
            *.gz|*.z)         gunzip -c "${src}" > "${file%.*}" || die "unpack failed: ${file}" ;;
            *.bz2)            bunzip2 -c "${src}" > "${file%.*}" || die "unpack failed: ${file}" ;;
            *.zip|*.jar)      unzip -qo "${src}" || die "unpack failed: ${file}" ;;
            *)                die "unpack: unknown archive format: ${file}" ;;
        esac
    done
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
    local ED=$(__edest)
    into "${__E_DESTTREE:-/usr}"
    install -d "${ED}/${__E_DESTTREE#/}/bin"
    install -m0755 "$@" "${ED}/${__E_DESTTREE#/}/bin/" || __helpers_die "${FUNCNAME} failed"
}

dosbin() {
    local ED=$(__edest)
    into "${__E_DESTTREE:-/usr}"
    install -d "${ED}/${__E_DESTTREE#/}/sbin"
    install -m0755 "$@" "${ED}/${__E_DESTTREE#/}/sbin/" || __helpers_die "${FUNCNAME} failed"
}

doexe() {
    local ED=$(__edest)
    local dir="${ED}/${__E_EXEDESTTREE#/}"
    install -d "${dir}"
    install ${EXEOPTIONS:--m0755} "$@" "${dir}/" || __helpers_die "${FUNCNAME} failed"
}

doins() {
    local ED=$(__edest) recursive=
    [[ $1 == -r ]] && { recursive=1; shift; }
    local dir="${ED}/${__E_INSDESTTREE#/}"
    install -d "${dir}"
    local f
    for f in "$@"; do
        if [[ -d ${f} && -n ${recursive} ]]; then
            cp -R "${f}" "${dir}/" || __helpers_die "${FUNCNAME} failed"
        else
            install ${INSOPTIONS:--m0644} "${f}" "${dir}/" || __helpers_die "${FUNCNAME} failed"
        fi
    done
}

dolib() {
    local ED=$(__edest) libdir
    libdir=$(get_libdir 2>/dev/null) || libdir=lib
    into "${__E_DESTTREE:-/usr}"
    local dir="${ED}/${__E_DESTTREE#/}/${libdir}"
    install -d "${dir}"
    install ${LIBOPTIONS:--m0644} "$@" "${dir}/" || __helpers_die "${FUNCNAME} failed"
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

doman() {
    local ED=$(__edest) i name suffix dir
    for i in "$@"; do
        name=${i##*/}
        suffix=${name##*.}
        dir="${ED}/usr/share/man/man${suffix:0:1}"
        install -d "${dir}"
        install -m0644 "${i}" "${dir}/${name}" || __helpers_die "${FUNCNAME} failed"
    done
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
    ___eapi_has_dosym_r && [[ $1 == -r ]] && shift
    local ED=$(__edest)
    [[ $# -eq 2 ]] || die "dosym: expected two arguments"
    local target=$1 link=$2
    install -d "${ED}/$(dirname "${link#/}")"
    ln -snf "${target}" "${ED}/${link#/}" || __helpers_die "${FUNCNAME} failed"
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
    local ED=$(__edest) i name dir
    for i in "$@"; do
        name=${i##*/}
        dir="${ED}/usr/share/locale/${name%.mo}/LC_MESSAGES"
        install -d "${dir}"
        install -m0644 "${i}" "${dir}/${PN}.mo" || __helpers_die "${FUNCNAME} failed"
    done
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
    local ED=$(__edest) mode=$1
    shift
    chmod "${mode}" "${@/#/${ED}/}" || __helpers_die "${FUNCNAME} failed"
}
fowners() {
    local ED=$(__edest) owner=$1
    shift
    chown "${owner}" "${@/#/${ED}/}" || __helpers_die "${FUNCNAME} failed"
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
        cp -f "${src}" "${tmp}" || __helpers_die "${helper%% *} failed"
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
