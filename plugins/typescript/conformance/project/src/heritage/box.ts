// a cross-file `implements` with a generic head, and a subclass.
import { Container, Item } from "./container";

export class Box implements Container<Item> {
  size(): number {
    return 0;
  }
}

export class SpecialBox extends Box {}
