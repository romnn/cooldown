import * as path from "@std/path";
import { format } from "@std/semver";
import lodash from "lodash";
import chalk from "chalk";

console.log(path.join("a", "b"), format, lodash.chunk([1, 2, 3], 2), chalk.green("ok"));
