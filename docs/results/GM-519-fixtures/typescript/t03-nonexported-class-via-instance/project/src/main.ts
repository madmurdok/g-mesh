import { service } from "./svc";
export function boot(): string {
  return service.start();
}
