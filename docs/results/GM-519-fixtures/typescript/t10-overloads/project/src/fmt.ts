export class Formatter {
  format(v: string): string;
  format(v: number): string;
  format(v: string | number): string {
    return String(v);
  }
}
export function format(v: string): string {
  return v;
}
