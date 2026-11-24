import { useMutation } from "@tanstack/react-query";
import { invoke } from "../lib/ipc";

type OwnershipAction =
  | { kind: "auto"; profile: string; enabled: boolean }
  | { kind: "once"; profile: string };

export function useOwnershipAction(refetch: () => Promise<unknown>) {
  return useMutation({
    mutationFn: (action: OwnershipAction) => action.kind === "auto"
      ? invoke<void>("ownership_set_auto", { profile: action.profile, enabled: action.enabled })
      : invoke<void>("ownership_refresh", { profile: action.profile }),
    onSettled: async () => { await refetch(); },
  });
}
