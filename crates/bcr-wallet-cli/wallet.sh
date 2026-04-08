#!/usr/bin/env zsh
set -o pipefail

BCR="../../target/debug/bcr-wallet-cli"
WALLET=$WALLET

bold=$'\e[1m'
dim=$'\e[2m'
green=$'\e[32m'
cyan=$'\e[36m'
yellow=$'\e[33m'
red=$'\e[31m'
reset=$'\e[0m'

ENTRIES=("${(@f)$("$BCR" help | awk '/Commands:/{f=1;next} f && NF {printf "%-20s %s\n", $1, substr($0, index($0,$2))}')}")

extract_args() {
  local cmd="$1"
  local usage
  usage=$("$BCR" help "$cmd" | sed -n 's/.*Usage:.*'"$cmd"'//p')
  echo "$usage" | grep -o '<[^>]\+>' | tr -d '<>' || true
  echo "$usage" | grep -o '\[[A-Z_]\+\]' | tr -d '[]' | sed 's/^/?/' || true
}

echo "${bold}${cyan}bcr-wallet-cli${reset} ${dim}(wallet: ${WALLET})${reset}"
echo

while true; do
  entry=$(printf "%s\n" "${ENTRIES[@]}" \
    | fzf --prompt="bcr(${WALLET})> " \
          --height=50% \
          --border=rounded \
          --preview="$BCR help {1}" \
          --preview-window=right:50%:wrap \
          --exit-0) || break

  cmd=${entry%% *}
  [[ -z "$cmd" ]] && break

  args=()

  for arg in $(extract_args "$cmd"); do
    local value=""
    if [[ "$arg" == \?* ]]; then
      vared -p "${yellow}${arg#?}${dim} (optional)${reset}: " -c value
      [[ -n "$value" ]] && args+=("$value")
    else
      vared -p "${green}${arg}${reset}: " -c value
      args+=("$value")
    fi
  done

  echo
  echo "${dim}→ ${BCR} --wallet ${WALLET} ${cmd} ${args[*]}${reset}"
  echo

  "$BCR" --wallet "$WALLET" "$cmd" "${args[@]}" || echo "${red}command failed${reset}"

  echo
  read -k 1 "?${dim}press any key to continue…${reset}"
  echo
done
