import { total } from "./util.js";

export class Cart {
  constructor(items) {
    this.items = items;
  }

  checkout() {
    const amount = total(this.items);
    return { amount, count: this.items.length };
  }
}
