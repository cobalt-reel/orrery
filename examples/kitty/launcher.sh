#!/bin/sh
# A launcher: fzf over the desktop's applications and the commands on PATH, in a kitty panel over everything that
# takes the keyboard (orrery.kdl binds this to Mod+D). Enter starts the choice through `orrery msg spawn`, so it opens
# as a new tab in the focused slot like any launch; Escape closes it. Needs fzf.
#
#   launcher.sh            open the panel (it runs `launcher.sh --pick` inside)
#   launcher.sh --pick     the picker itself, in this terminal
#   launcher.sh --list     print the entries: NAME <tab> KIND <tab> COMMAND, KIND being app, term or cmd
#
# Applications are the desktop entries in $XDG_DATA_HOME and $XDG_DATA_DIRS (`applications/`, earlier directories
# winning for one entry), without hidden ones, ones whose TryExec is missing and ones OnlyShowIn/NotShowIn keep
# from $XDG_CURRENT_DESKTOP; their Exec line, unescaped, runs with `sh -c`, its field codes (%f, %U, ...) left out;
# Terminal=true ones run in a new kitty. $ORRERY names orrery (orrery sets it; default: `orrery` on PATH).
set -eu

orrery=${ORRERY:-orrery}
tab=$(printf '\t')

list_apps() {
    data_home=${XDG_DATA_HOME:-$HOME/.local/share}
    data_dirs=${XDG_DATA_DIRS:-/usr/local/share:/usr/share}
    old_ifs=$IFS
    IFS=:
    # Each file as DIR <tab> FILE, directories in order of precedence.
    for d in $data_home $data_dirs; do
        [ -d "$d/applications" ] || continue
        find -L "$d/applications" -name '*.desktop' -type f 2>/dev/null | sort | while IFS= read -r f; do
            printf '%s\t%s\n' "$d/applications/" "$f"
        done
    done
    IFS=$old_ifs
}

# DIR <tab> FILE lines in, NAME <tab> KIND <tab> COMMAND lines out (the first file of each desktop file ID only).
read_entries() {
    awk -F '\t' -v desktops="${XDG_CURRENT_DESKTOP:-}" '
    # A desktop file string: \s \n \t \r \\ are its escapes (the controls become spaces: one entry is one line).
    function unescape(v,    out, i, c, n) {
        out = ""
        for (i = 1; i <= length(v); i++) {
            c = substr(v, i, 1)
            if (c == "\\" && i < length(v)) {
                n = substr(v, ++i, 1)
                if (n == "s" || n == "n" || n == "t" || n == "r") c = " "
                else if (n == "\\") c = "\\"
                else c = c n
            }
            out = out c
        }
        gsub(/[\t\r\n]/, " ", out)
        return out
    }
    # TryExec: a path that is executable, or a name found on PATH.
    function found(p,    q) {
        q = "\047"
        if (index(p, q)) return 0
        if (index(p, "/")) return system("test -x " q p q) == 0
        return system("command -v " q p q " >/dev/null 2>&1") == 0
    }
    # Whether a ;-separated OnlyShowIn/NotShowIn list names one of $XDG_CURRENT_DESKTOP (:-separated).
    function names_ours(list,    n, a, i) {
        n = split(list, a, ";")
        for (i = 1; i <= n; i++)
            if (a[i] != "" && index(":" desktops ":", ":" a[i] ":")) return 1
        return 0
    }
    {
        id = substr($2, length($1) + 1)
        gsub("/", "-", id)
        if (id in seen) next
        seen[id] = 1
        file = $2
        group = ""; name = ""; exec_ = ""; type = ""; tryexec = ""; only = ""; not_ = ""; hidden = 0; term = 0
        while ((getline line < file) > 0) {
            sub(/\r$/, "", line)
            if (line ~ /^\[/) { group = line; continue }
            if (group != "[Desktop Entry]") continue
            eq = index(line, "=")
            if (eq == 0) continue
            key = substr(line, 1, eq - 1); value = substr(line, eq + 1)
            sub(/[ \t]+$/, "", key); sub(/^[ \t]+/, "", value)
            if (key == "Name") name = unescape(value)
            else if (key == "Exec") exec_ = unescape(value)
            else if (key == "Type") type = value
            else if (key == "TryExec") tryexec = unescape(value)
            else if (key == "OnlyShowIn") only = value
            else if (key == "NotShowIn") not_ = value
            else if ((key == "NoDisplay" || key == "Hidden") && value == "true") hidden = 1
            else if (key == "Terminal" && value == "true") term = 1
        }
        close(file)
        if (hidden || name == "" || exec_ == "" || (type != "" && type != "Application")) next
        if ((only != "" && !names_ours(only)) || names_ours(not_)) next
        if (tryexec != "" && !found(tryexec)) next
        # Field codes: %% is a literal %, every other %X goes, with the space before it and quotes round it.
        gsub(/%%/, "\001", exec_)
        gsub(/[ \t]*"%[a-zA-Z]"/, "", exec_)
        gsub(/[ \t]*%[a-zA-Z]/, "", exec_)
        gsub("\001", "%", exec_)
        sub(/[ \t]+$/, "", exec_)
        printf "%s\t%s\t%s\n", name, (term ? "term" : "app"), exec_
    }'
}

list_commands() {
    old_ifs=$IFS
    IFS=:
    for d in $PATH; do
        [ -d "$d" ] || continue
        find -L "$d" -maxdepth 1 -type f -perm -100 2>/dev/null
    done | sed 's|.*/||' | sort -u | awk '{ printf "%s\tcmd\t%s\n", $0, $0 }'
    IFS=$old_ifs
}

case ${1:-} in
--list)
    list_apps | read_entries | sort -f
    list_commands
    ;;
--pick)
    command -v fzf >/dev/null 2>&1 || { echo "launcher.sh: needs fzf" >&2; sleep 3; exit 1; }
    choice=$("$0" --list | fzf --delimiter="$tab" --with-nth=1,2 --nth=1 --tiebreak=index --prompt='run: ' \
        --layout=reverse --info=inline) || exit 0
    kind=$(printf '%s\n' "$choice" | cut -f 2)
    command=$(printf '%s\n' "$choice" | cut -f 3-)
    case $kind in
    app) "$orrery" msg spawn sh -c "$command" >/dev/null ;;
    term) "$orrery" msg spawn kitty sh -c "$command" >/dev/null ;;
    cmd) "$orrery" msg spawn "$command" >/dev/null ;;
    esac
    ;;
'')
    exec kitten panel --edge=center-sized --lines=20 --columns=80 --layer=overlay --focus-policy=exclusive \
        --class=orrery-launcher "$0" --pick
    ;;
*)
    echo "usage: launcher.sh [--pick | --list]" >&2
    exit 2
    ;;
esac
