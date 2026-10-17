import { Cart } from "./cart.js";

function submitOrder(items) {
  const cart = new Cart(items);
  return cart.checkout();
}

try {
  console.log(submitOrder([{ price: 10, quantity: "2" }]));
  submitOrder([{ price: 3, quantity: "many" }]);
} catch (error) {
  console.log(error.stack);
}
