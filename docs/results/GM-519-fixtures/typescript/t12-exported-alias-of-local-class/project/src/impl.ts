class Engine {
  ignite(): string {
    return "vroom";
  }
}
export type Motor = Engine;
export function build(): Motor {
  return new Engine();
}
