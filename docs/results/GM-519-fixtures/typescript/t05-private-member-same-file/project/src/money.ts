export class Money {
  constructor(private cents: number) {}
  private raw(): number {
    return this.cents;
  }
  equals(other: Money): boolean {
    return other.raw() === this.raw();
  }
}
