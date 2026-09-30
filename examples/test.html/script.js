const interval = setInterval(() => {
  console.log("tick", new Date().toISOString());
  document.getElementById("time").innerHTML = new Date().toISOString();
}, 1000);
