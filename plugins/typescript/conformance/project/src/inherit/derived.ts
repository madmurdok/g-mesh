import { Base } from "./base";

/** Calls the inherited method through `this`. */
export class Child extends Base {
  viaThis(): string {
    return this.hello();
  }
}

/** Calls the inherited method through `super`, with no override of its own. */
export class Other extends Base {
  viaSuper(): string {
    return super.hello();
  }
}
