import { secretValue } from "#int/secret";

export function usePrivate(): number {
  return secretValue();
}
