/**
 * A decorator is a plain function, indexed under its own name. A use site
 * spells it `@Component`, and `find_definition("@Component")` must find this
 * declaration through `plugin.toml`'s `[plugin.symbol_query_prefixes]`
 * (conformance/expect.toml).
 */
export function Component(target: Function): void {
  void target;
}

@Component
export class Panel {}
