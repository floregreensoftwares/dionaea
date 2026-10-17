export function parseQuantity(input) {
  const value = Number(input);
  if (Number.isNaN(value)) {
    throw new TypeError("quantity is not a number: " + input);
  }
  return value;
}

export function total(items) {
  return items.reduce((sum, item) => sum + item.price * parseQuantity(item.quantity), 0);
}
