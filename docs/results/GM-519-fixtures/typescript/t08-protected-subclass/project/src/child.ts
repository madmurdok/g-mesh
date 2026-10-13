import { Base } from "./base";
export class Child extends Base {
  run(): string {
    return this.log();
  }
}
